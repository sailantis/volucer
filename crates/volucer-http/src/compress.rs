//! On-the-fly compression for responses the server produces itself.
//!
//! The static layer already serves a *precompressed* `.br`/`.gz` variant when one exists, and
//! that remains the right answer for a build artefact: it costs nothing per request. This
//! module exists for what no build step can precompress — a backend's output, which does not
//! exist until the request runs. If it is not compressed here, it is not compressed at all.
//!
//! # What it deliberately does not do
//!
//! - **Brotli.** The `brotli` crate defaults to quality 11, which is a build-time setting
//!   misapplied to a request path: the well-known `tower-http` issue took 8.3 s for a 770 KiB
//!   body at that level. Brotli belongs offline, at high quality, producing the `.br` files
//!   [`crate::static_files`] already serves. gzip and zstd cover the dynamic case.
//! - **Anything already encoded.** A response carrying `Content-Encoding` is passed through:
//!   compressing twice makes it larger and changes what the header means.
//! - **Ranged responses.** `Content-Range` means the bytes are a window into another
//!   representation; compressing them would describe a different one.
//! - **Small bodies.** Below [`MIN_LENGTH`] the framing cost exceeds the saving.
//! - **Content that is already compressed.** A `.png` or `.zip` deflates to about its own
//!   size, having spent the CPU to find out.
//!
//! # The zstd window, which is not optional
//!
//! Browsers refuse a zstd frame whose window exceeds 8 MiB. Measured here, zstd's *default*
//! window stays at or below 8 MiB for levels 1–19 even on a 32 MiB input, and only reaches
//! 16 MiB+ at the ultra levels (which switch to single-segment framing, where the window is the
//! whole body). So the limit is not reached at the levels this module uses — but it is set
//! explicitly anyway, because a property that costs one line must not rest on a dependency's
//! default.

use bytes::Bytes;
use hyper::header::{
    HeaderMap, HeaderValue, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, VARY,
};
use std::io::Write;

/// Below this, compressing costs more than it saves.
///
/// Chosen from the trade rather than measured on this host: a deflate stream's framing is a
/// few tens of bytes and the CPU is a few microseconds, so a body under about this size gains
/// nothing a client would notice. It is configuration-visible through `min_length`.
pub const MIN_LENGTH: usize = 1024;

/// The largest zstd window a browser will accept, as a window log.
///
/// 2^23 = 8 MiB. See the module documentation: zstd's defaults stay under this at the levels
/// used here, and the clamp exists so that stays true if a level is ever raised.
const MAX_WINDOW_LOG: u32 = 23;

/// Which encodings a deployment offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// Every client accepts it. The safe default.
    Gzip,
    /// Smaller than gzip for text and supported by current browsers; the window is clamped.
    Zstd,
}

impl Encoding {
    /// The token as it appears in `Content-Encoding` and `Accept-Encoding`.
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Gzip => "gzip",
            Encoding::Zstd => "zstd",
        }
    }
}

/// What a caller asked for, already reduced to a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Whether compression is offered at all.
    pub enabled: bool,
    /// Encodings in preference order. Ignored when `enabled` is false.
    pub encodings: [Option<Encoding>; 2],
    /// Bodies below this are passed through.
    pub min_length: usize,
    /// The level handed to the encoder, 1–9 for gzip and 1–19 for zstd.
    pub level: u32,
}

impl From<&volucer_core::config::CompressionSetting> for Settings {
    /// Translate the configuration into the runtime decision, once per request.
    ///
    /// The two types exist for the reason the rest of this crate keeps its config types apart:
    /// `volucer-core` is the leaf and cannot depend on `serde`-free request machinery, and this
    /// module must not grow a dependency on the config graph. The translation is where an
    /// unknown encoding name is dropped rather than defaulted ? a typo in `encodings` must not
    /// silently become gzip, because the operator may have written `br` believing it would
    /// produce brotli.
    fn from(setting: &volucer_core::config::CompressionSetting) -> Self {
        let mut encodings = [None, None];
        let mut slot = 0;
        for name in &setting.encodings {
            if slot == encodings.len() {
                break;
            }
            let encoding = match name.to_ascii_lowercase().as_str() {
                "gzip" => Some(Encoding::Gzip),
                "zstd" => Some(Encoding::Zstd),
                _ => None,
            };
            if let Some(encoding) = encoding {
                encodings[slot] = Some(encoding);
                slot += 1;
            }
        }

        Self {
            enabled: setting.enabled,
            encodings,
            min_length: setting.min_length.unwrap_or(MIN_LENGTH),
            level: setting.level,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            // gzip first: it is the one encoding every client in existence accepts, so a
            // client offering both gets the answer with no compatibility surface at all.
            // zstd is offered because it is smaller and the user asked for it; the order is
            // `q`-value aware, so an explicit preference in the request still wins.
            encodings: [Some(Encoding::Gzip), Some(Encoding::Zstd)],
            min_length: MIN_LENGTH,
            level: 6,
        }
    }
}

/// A compressed body, or the reason there is none.
#[derive(Debug)]
pub enum Outcome {
    /// The body was compressed, with the encoding used.
    Compressed(Bytes, Encoding),
    /// The body is passed through unchanged.
    Unchanged,
}

/// Compress `body` for this request, or say why not.
///
/// Every refusal is silent by design: a response that was not compressed is still a correct
/// response, and an error here would turn an optimisation into an outage.
pub fn compress(
    settings: &Settings,
    request_headers: &HeaderMap,
    response_headers: &HeaderMap,
    content_type: Option<&str>,
    body: Bytes,
) -> Outcome {
    if !settings.enabled || body.len() < settings.min_length {
        return Outcome::Unchanged;
    }
    // Already encoded, or a window into a different representation: both would be made wrong
    // by compressing, not merely redundant.
    if response_headers.contains_key(CONTENT_ENCODING)
        || response_headers.contains_key(CONTENT_RANGE)
    {
        return Outcome::Unchanged;
    }
    if content_type.is_some_and(already_compressed) {
        return Outcome::Unchanged;
    }
    let Some(accept) = request_headers
        .get(ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
    else {
        return Outcome::Unchanged;
    };

    let Some(encoding) = choose(settings, accept) else {
        return Outcome::Unchanged;
    };

    match encode(encoding, settings.level, &body) {
        // A compressor that makes the body larger has lost; sending the original is both
        // smaller and honest about what the client received.
        Some(compressed) if compressed.len() < body.len() => {
            Outcome::Compressed(compressed, encoding)
        }
        _ => Outcome::Unchanged,
    }
}

/// The encoding to use, honouring `q`-values and the deployment's preference order.
///
/// `q=0` is a refusal, which is the part a naive `contains("gzip")` gets wrong: a client
/// sending `gzip;q=0, identity` must not be sent gzip. `*` is treated as accepting whatever
/// the deployment offers at that quality.
pub fn choose(settings: &Settings, accept: &str) -> Option<Encoding> {
    let mut best: Option<(f32, usize, Encoding)> = None;

    for (position, encoding) in settings.encodings.iter().enumerate() {
        let Some(encoding) = encoding else { continue };
        let Some(quality) = quality_of(accept, encoding.as_str()) else {
            continue;
        };
        if quality <= 0.0 {
            continue;
        }
        // Ties are broken by the deployment's order, so `encodings = ["gzip", "zstd"]` means
        // exactly that when a client accepts both equally. Comparing positions rather than the
        // encodings themselves avoids a second opinion about a preference already stated.
        let better = match best {
            None => true,
            Some((existing, existing_position, _)) => {
                quality > existing || (quality == existing && position < existing_position)
            }
        };
        if better {
            best = Some((quality, position, *encoding));
        }
    }

    best.map(|(_, _, encoding)| encoding)
}

/// The quality this `Accept-Encoding` value assigns to `name`, or `None` if absent.
///
/// A missing parameter means `q=1`, per RFC 9110. An explicit `*` entry is consulted when the
/// name itself is absent.
fn quality_of(accept: &str, name: &str) -> Option<f32> {
    let mut wildcard: Option<f32> = None;
    let mut exact: Option<f32> = None;

    for part in accept.split(',') {
        let mut fields = part.split(';');
        let token = fields.next().unwrap_or_default().trim();
        if token.is_empty() {
            continue;
        }
        let quality = fields
            .find_map(|field| {
                let field = field.trim();
                field
                    .strip_prefix("q=")
                    .or_else(|| field.strip_prefix("Q="))
                    .and_then(|value| value.trim().parse::<f32>().ok())
            })
            .unwrap_or(1.0);
        if token.eq_ignore_ascii_case(name) {
            exact = Some(quality);
        } else if token == "*" {
            wildcard = Some(quality);
        }
    }

    exact.or(wildcard)
}

/// Whether a media type is one that compression cannot help.
///
/// Not a blocklist of "bad" types: it is the set that is *already* entropy-coded, where the
/// only thing a second pass produces is CPU heat.
fn already_compressed(content_type: &str) -> bool {
    let media = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    media.starts_with("image/")
        && !matches!(
            media.as_str(),
            "image/svg+xml" | "image/x-icon" | "image/vnd.microsoft.icon"
        )
        || media.starts_with("video/")
        || media.starts_with("audio/")
        || media.starts_with("font/")
        || media == "application/zip"
        || media == "application/gzip"
        || media == "application/x-gzip"
        || media == "application/zstd"
        || media == "application/x-zstd"
        || media == "application/x-tar"
        || media == "application/x-7z-compressed"
        || media == "application/x-rar-compressed"
        || media == "application/pdf"
        || media == "application/wasm"
        || media == "application/grpc"
        || media.starts_with("application/grpc+")
}

/// Run the encoder. `None` means the encoder failed, which is never worth failing a request
/// over.
fn encode(encoding: Encoding, level: u32, body: &[u8]) -> Option<Bytes> {
    match encoding {
        Encoding::Gzip => {
            use flate2::{write::GzEncoder, Compression};
            let mut encoder = GzEncoder::new(
                Vec::with_capacity(body.len() / 2),
                Compression::new(level.clamp(1, 9)),
            );
            encoder.write_all(body).ok()?;
            encoder.finish().ok().map(Bytes::from)
        }
        Encoding::Zstd => {
            let mut compressor = zstd::bulk::Compressor::new(level.clamp(1, 19) as i32).ok()?;
            compressor
                .set_parameter(zstd::zstd_safe::CParameter::WindowLog(MAX_WINDOW_LOG))
                .ok()?;
            compressor.compress(body).ok().map(Bytes::from)
        }
    }
}

/// Apply the result of [`compress`] to the response headers.
///
/// `Content-Length` is replaced rather than removed: the length of the compressed body is
/// known, and a response with no length forces the client to read until the connection closes.
pub fn apply(headers: &mut HeaderMap, outcome: &Outcome) {
    let Outcome::Compressed(body, encoding) = outcome else {
        return;
    };
    headers.insert(
        CONTENT_ENCODING,
        HeaderValue::from_static(encoding.as_str()),
    );
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).expect("a decimal length is a valid value"),
    );
    // The same URL now answers differently by encoding, so a cache must key on it — without
    // this a proxy that stored the gzip answer could hand it to a client that cannot read it.
    append_vary(headers);
}

/// The `Vary` value a compressed response should carry, given the application's own headers.
///
/// Returned rather than inserted so the caller keeps its ordered list: a `HeaderMap` would
/// collapse anything the application repeated, which is the bug this shape avoids.
pub fn vary_value(existing: &[(hyper::header::HeaderName, HeaderValue)]) -> Option<HeaderValue> {
    let mut parts: Vec<String> = Vec::new();
    for (name, value) in existing {
        if *name != VARY {
            continue;
        }
        let Ok(text) = value.to_str() else { continue };
        if text.split(',').any(|part| part.trim() == "*") {
            // `Vary: *` already means "varies by everything"; a token would be noise.
            return None;
        }
        parts.push(text.to_string());
    }

    if parts.iter().any(|text| {
        text.split(',')
            .any(|p| p.trim().eq_ignore_ascii_case("accept-encoding"))
    }) {
        return None;
    }

    let combined = if parts.is_empty() {
        "accept-encoding".to_string()
    } else {
        format!("{}, accept-encoding", parts.join(", "))
    };
    HeaderValue::from_str(&combined).ok()
}

/// Add `accept-encoding` to `Vary`, preserving whatever the application already sent.
pub fn append_vary(headers: &mut HeaderMap) {
    let existing = headers
        .get(VARY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    // A `Vary: *` already means "varies by everything"; adding a token would be noise.
    if existing.split(',').any(|part| part.trim() == "*") {
        return;
    }
    if existing
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case("accept-encoding"))
    {
        return;
    }

    let combined = if existing.is_empty() {
        "accept-encoding".to_string()
    } else {
        format!("{existing}, accept-encoding")
    };
    if let Ok(value) = HeaderValue::from_str(&combined) {
        headers.insert(VARY, value);
    }
}

/// The content type a response advertised, for the "already compressed" test.
pub fn content_type_of(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).expect("a valid value"));
        }
        map
    }

    fn accepting(value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_str(value).expect("valid"),
        );
        map
    }

    fn enabled() -> Settings {
        Settings {
            enabled: true,
            ..Settings::default()
        }
    }

    #[test]
    fn q_zero_is_a_refusal_not_a_miss() {
        // The part a `contains("gzip")` check gets wrong, and the reason it matters: this
        // client has explicitly said it cannot accept gzip.
        assert_eq!(None, choose(&enabled(), "gzip;q=0"));
        assert_eq!(None, choose(&enabled(), "gzip;q=0, identity;q=1"));
        // And a different encoding still wins when it is acceptable.
        assert_eq!(
            Some(Encoding::Zstd),
            choose(&enabled(), "gzip;q=0, zstd;q=1")
        );
    }

    #[test]
    fn a_missing_quality_means_one() {
        assert_eq!(Some(Encoding::Gzip), choose(&enabled(), "gzip"));
        assert_eq!(Some(Encoding::Gzip), choose(&enabled(), "deflate, gzip"));
    }

    #[test]
    fn the_deployments_order_breaks_a_tie() {
        // Both acceptable at q=1: the configured order decides, so the answer is the same on
        // every request rather than whichever the parser happened to see first.
        let gzip_first = Settings {
            enabled: true,
            encodings: [Some(Encoding::Gzip), Some(Encoding::Zstd)],
            ..Settings::default()
        };
        let zstd_first = Settings {
            enabled: true,
            encodings: [Some(Encoding::Zstd), Some(Encoding::Gzip)],
            ..Settings::default()
        };
        assert_eq!(Some(Encoding::Gzip), choose(&gzip_first, "gzip, zstd"));
        assert_eq!(Some(Encoding::Zstd), choose(&zstd_first, "gzip, zstd"));
    }

    #[test]
    fn an_explicit_preference_beats_the_deployments_order() {
        // A client saying "zstd is better" is better informed about its own end than the
        // deployment is.
        assert_eq!(
            Some(Encoding::Zstd),
            choose(&enabled(), "gzip;q=0.5, zstd;q=1.0")
        );
        assert_eq!(
            Some(Encoding::Gzip),
            choose(&enabled(), "gzip;q=1.0, zstd;q=0.1")
        );
    }

    #[test]
    fn a_wildcard_accepts_what_the_deployment_offers() {
        assert_eq!(Some(Encoding::Gzip), choose(&enabled(), "*"));
        assert_eq!(None, choose(&enabled(), "*;q=0"));
    }

    #[test]
    fn an_encoding_the_deployment_does_not_offer_is_never_chosen() {
        let gzip_only = Settings {
            enabled: true,
            encodings: [Some(Encoding::Gzip), None],
            ..Settings::default()
        };
        assert_eq!(None, choose(&gzip_only, "zstd"));
        assert_eq!(Some(Encoding::Gzip), choose(&gzip_only, "zstd, gzip"));
    }

    #[test]
    fn the_threshold_is_what_stops_a_body_that_would_have_compressed() {
        // The isolation the end-to-end test cannot give: this body is highly compressible, so
        // the "did it actually get smaller" check would *accept* it. Only `min_length` can
        // refuse it, which makes the assertion a test of the threshold rather than of the
        // outcome. Removing the threshold makes this fail; removing the size check does not.
        let text = "aaaa".repeat(200); // 800 bytes: below the 1024 default, compresses well
        assert!(text.len() < MIN_LENGTH);
        assert!(
            encode(Encoding::Gzip, 6, text.as_bytes())
                .is_some_and(|compressed| compressed.len() < text.len()),
            "the fixture must be one that compression would otherwise accept"
        );

        let settings = enabled();
        let outcome = compress(
            &settings,
            &accepting("gzip"),
            &HeaderMap::new(),
            Some("text/plain"),
            Bytes::copy_from_slice(text.as_bytes()),
        );
        assert!(
            matches!(outcome, Outcome::Unchanged),
            "a body below min_length must be passed through even when it would shrink"
        );

        // And with the threshold lowered below it, the same body *is* compressed ? so the
        // refusal above came from the threshold and not from something else.
        let lowered = Settings {
            min_length: 100,
            ..enabled()
        };
        let outcome = compress(
            &lowered,
            &accepting("gzip"),
            &HeaderMap::new(),
            Some("text/plain"),
            Bytes::copy_from_slice(text.as_bytes()),
        );
        assert!(
            matches!(outcome, Outcome::Compressed(..)),
            "with a lower threshold the same body compresses"
        );
    }

    #[test]
    fn a_small_body_is_passed_through() {
        let body = Bytes::from_static(b"tiny");
        assert!(matches!(
            compress(
                &enabled(),
                &accepting("gzip"),
                &HeaderMap::new(),
                Some("text/html"),
                body
            ),
            Outcome::Unchanged
        ));
    }

    #[test]
    fn an_already_encoded_response_is_not_encoded_again() {
        let body = Bytes::from(vec![b'a'; 4096]);
        let response = headers(&[("content-encoding", "br")]);
        assert!(matches!(
            compress(
                &enabled(),
                &accepting("gzip"),
                &response,
                Some("text/html"),
                body
            ),
            Outcome::Unchanged
        ));
    }

    #[test]
    fn a_ranged_response_is_never_compressed() {
        // The bytes are a window into another representation; compressing them would describe
        // a representation the client did not ask for.
        let body = Bytes::from(vec![b'a'; 4096]);
        let response = headers(&[("content-range", "bytes 0-99/1000")]);
        assert!(matches!(
            compress(
                &enabled(),
                &accepting("gzip"),
                &response,
                Some("text/plain"),
                body
            ),
            Outcome::Unchanged
        ));
    }

    #[test]
    fn content_that_is_already_entropy_coded_is_skipped() {
        for media in [
            "image/png",
            "image/jpeg",
            "application/zip",
            "video/mp4",
            "font/woff2",
            "application/grpc",
            "application/grpc+proto",
        ] {
            assert!(already_compressed(media), "{media} should be skipped");
        }
        // The two that are text despite being images.
        for media in ["image/svg+xml", "text/html", "application/json"] {
            assert!(!already_compressed(media), "{media} should be compressed");
        }
        // Parameters must not defeat the test.
        assert!(already_compressed("image/png; charset=binary"));
    }

    #[test]
    fn compression_that_makes_a_body_larger_is_abandoned() {
        // 4 KiB of random bytes does not compress; the honest answer is the original.
        let mut state = 0x2545F491u32;
        let body: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state & 0xff) as u8
            })
            .collect();
        let outcome = compress(
            &enabled(),
            &accepting("gzip"),
            &HeaderMap::new(),
            Some("application/octet-stream"),
            Bytes::from(body),
        );
        if let Outcome::Compressed(compressed, _) = &outcome {
            panic!(
                "incompressible data must not be sent compressed ({} bytes)",
                compressed.len()
            );
        }
    }

    #[test]
    fn a_text_body_is_actually_smaller_and_round_trips() {
        let text = "hello volucer ".repeat(400);
        let original = Bytes::copy_from_slice(text.as_bytes());
        for encoding in [Encoding::Gzip, Encoding::Zstd] {
            let Some(compressed) = encode(encoding, 6, &original) else {
                panic!("{encoding:?} should encode");
            };
            assert!(
                compressed.len() < original.len(),
                "{encoding:?} should shrink repetitive text: {} vs {}",
                compressed.len(),
                original.len()
            );
            // Both arms produce `Option<Vec<u8>>`: the point is the bytes, and an encoder
            // that cannot be read back is a failure however the decoder reports it.
            let round_tripped: Option<Vec<u8>> = match encoding {
                Encoding::Gzip => {
                    use flate2::read::GzDecoder;
                    use std::io::Read;
                    let mut out = Vec::new();
                    GzDecoder::new(&compressed[..])
                        .read_to_end(&mut out)
                        .ok()
                        .map(|_| out)
                }
                Encoding::Zstd => zstd::stream::decode_all(&compressed[..]).ok(),
            };
            assert_eq!(
                Some(original.to_vec()),
                round_tripped,
                "{encoding:?} must round-trip the bytes exactly"
            );
        }
    }

    #[test]
    fn the_zstd_window_stays_within_what_a_browser_accepts() {
        // The production trap: a frame whose window exceeds 8 MiB is refused by browsers, and
        // it only shows up on large bodies. A 4 MiB body at the default level is enough to
        // produce a multi-byte window descriptor, so the header is parsed rather than assumed.
        let body = vec![b'a'; 4 * 1024 * 1024];
        let frame = encode(Encoding::Zstd, 6, &body).expect("zstd encodes");

        let descriptor = *frame.get(4).expect("a frame header");
        let single_segment = (descriptor >> 5) & 1 == 1;
        if single_segment {
            // Single-segment frames carry no window descriptor; the window is the content size,
            // and the body is under the limit by construction.
            assert!(body.len() <= 1 << MAX_WINDOW_LOG);
            return;
        }
        let fcs_flag = descriptor >> 6;
        let index = 5 + if fcs_flag == 0 { 1 } else { 0 };
        let window_descriptor = *frame.get(index).expect("a window descriptor");
        let window_log = 10 + (window_descriptor as u32 >> 3);
        assert!(
            window_log <= MAX_WINDOW_LOG,
            "zstd window_log {window_log} exceeds the browser limit of {MAX_WINDOW_LOG}"
        );
    }

    #[test]
    fn vary_is_appended_rather_than_replaced() {
        // An application that already varies by `cookie` must keep that, or a cache starts
        // serving one user's response to another.
        let mut response = headers(&[("vary", "cookie")]);
        apply(
            &mut response,
            &Outcome::Compressed(Bytes::new(), Encoding::Gzip),
        );
        let vary = response.get(VARY).unwrap().to_str().unwrap();
        // The exact string, not two `contains` checks: the order and the separator are part of
        // what a cache parses, so a test that only looks for words would pass on a malformed
        // value. The `replace` this used to wrap it in was a no-op that clippy caught.
        assert_eq!("cookie, accept-encoding", vary);
    }

    #[test]
    fn vary_is_not_duplicated_or_added_to_a_wildcard() {
        let mut already = headers(&[("vary", "accept-encoding")]);
        apply(
            &mut already,
            &Outcome::Compressed(Bytes::new(), Encoding::Gzip),
        );
        assert_eq!("accept-encoding", already.get(VARY).unwrap());

        let mut star = headers(&[("vary", "*")]);
        apply(
            &mut star,
            &Outcome::Compressed(Bytes::new(), Encoding::Gzip),
        );
        assert_eq!("*", star.get(VARY).unwrap());
    }

    #[test]
    fn applying_the_outcome_sets_encoding_and_the_new_length() {
        let mut response = HeaderMap::new();
        let compressed = Bytes::from_static(b"12345");
        apply(
            &mut response,
            &Outcome::Compressed(compressed.clone(), Encoding::Zstd),
        );
        assert_eq!("zstd", response.get(CONTENT_ENCODING).unwrap());
        assert_eq!("5", response.get(CONTENT_LENGTH).unwrap());
        assert_eq!("accept-encoding", response.get(VARY).unwrap());
    }

    #[test]
    fn an_unchanged_outcome_touches_no_header() {
        let mut response = headers(&[("content-type", "text/html")]);
        let before = response.clone();
        apply(&mut response, &Outcome::Unchanged);
        assert_eq!(
            before, response,
            "a pass-through must not alter the response"
        );
    }

    #[test]
    fn disabled_means_disabled() {
        let body = Bytes::from(vec![b'a'; 100_000]);
        let settings = Settings {
            enabled: false,
            ..Settings::default()
        };
        assert!(matches!(
            compress(
                &settings,
                &accepting("gzip"),
                &HeaderMap::new(),
                Some("text/html"),
                body
            ),
            Outcome::Unchanged
        ));
    }
}
