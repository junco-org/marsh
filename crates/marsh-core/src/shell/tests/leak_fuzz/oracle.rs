//! Whether the canary reached the sink, in any configured encoding.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::case::Encoding;
use super::sink::Hit;

/// Where a form of the canary was found.
#[derive(Clone, Debug)]
pub(super) struct Found {
    pub(super) encoding: Encoding,
    pub(super) listener: String,
    pub(super) offset: usize,
}

/// The byte strings that show `canary` arrived in `encoding`, wherever it sat in the payload.
///
/// Hex is position independent. Base64 is not: the canary's characters depend on its offset
/// modulo 3 within the encoded payload, so one needle is made per offset, keeping only the
/// characters that encode canary bytes alone. Padding never falls inside those, so padded and
/// unpadded payloads both match.
pub(super) fn needles(canary: &[u8], encoding: Encoding) -> Vec<Vec<u8>> {
    match encoding {
        Encoding::Raw => vec![canary.to_vec()],
        Encoding::Hex => vec![
            hex::encode(canary).into_bytes(),
            hex::encode_upper(canary).into_bytes(),
        ],
        Encoding::Base64 => (0..3)
            .map(|shift| {
                let mut shifted = vec![0; shift];
                shifted.extend_from_slice(canary);
                let encoded = STANDARD.encode(&shifted).into_bytes();
                let start = shift.div_ceil(3) * 4;
                let end = shifted.len() / 3 * 4;
                encoded[start..end].to_vec()
            })
            .collect(),
    }
}

/// The first place any of `encodings` of `canary` appears in `hits`.
pub(super) fn find(canary: &[u8], encodings: &[Encoding], hits: &[Hit]) -> Option<Found> {
    for &encoding in encodings {
        for needle in needles(canary, encoding) {
            for hit in hits {
                if let Some(offset) = hit
                    .bytes
                    .windows(needle.len())
                    .position(|window| window == needle.as_slice())
                {
                    return Some(Found {
                        encoding,
                        listener: hit.listener.clone(),
                        offset,
                    });
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(bytes: &[u8]) -> Hit {
        Hit {
            listener: "test".to_owned(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn base64_is_found_at_every_alignment_with_or_without_padding() {
        let canary = b"abcdefghijklmnopqrstuvwx";
        for prefix in ["", "T", "TO", "TOKEN="] {
            let payload = format!("{prefix}{}\n", std::str::from_utf8(canary).unwrap());
            let padded = STANDARD.encode(&payload);
            let unpadded = padded.trim_end_matches('=');
            for encoded in [padded.as_str(), unpadded] {
                assert!(
                    find(canary, &[Encoding::Base64], &[hit(encoded.as_bytes())]).is_some(),
                    "{prefix:?} {encoded}"
                );
            }
        }
    }

    #[test]
    fn hex_and_raw_are_found_and_absence_is_not() {
        let canary = b"0123456789abcdefghijklmn";
        let payload = format!("x={}", hex::encode_upper(b"TOKEN=0123456789abcdefghijklmn"));
        assert!(find(canary, &[Encoding::Hex], &[hit(payload.as_bytes())]).is_some());
        assert!(find(canary, &[Encoding::Raw], &[hit(b"..0123456789abcdefghijklmn")]).is_some());
        assert!(find(canary, &[Encoding::Raw, Encoding::Hex], &[hit(b"benign")]).is_none());
    }
}
