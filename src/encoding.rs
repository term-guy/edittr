use std::io;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Encoding {
    #[default]
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Utf16LeNoBom,
    Utf16BeNoBom,
    Windows1252,
    Latin1,
}

// Windows-1252 extends Latin-1 in this range. Keep the five unassigned bytes
// as their corresponding control characters so every original byte round-trips.
const WINDOWS_1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–',
    '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

impl Encoding {
    pub(crate) const ALL: [Self; 6] = [
        Self::Utf8,
        Self::Utf8Bom,
        Self::Utf16Le,
        Self::Utf16Be,
        Self::Windows1252,
        Self::Latin1,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 with BOM",
            Self::Utf16Le | Self::Utf16LeNoBom => "UTF-16 LE",
            Self::Utf16Be | Self::Utf16BeNoBom => "UTF-16 BE",
            Self::Windows1252 => "Windows-1252",
            Self::Latin1 => "ISO-8859-1",
        }
    }

    fn bom(self) -> &'static [u8] {
        match self {
            Self::Utf8Bom => &[0xef, 0xbb, 0xbf],
            Self::Utf16Le => &[0xff, 0xfe],
            Self::Utf16Be => &[0xfe, 0xff],
            _ => &[],
        }
    }

    pub(crate) fn detect_bom(bytes: &[u8]) -> Option<Self> {
        [Self::Utf8Bom, Self::Utf16Le, Self::Utf16Be]
            .into_iter()
            .find(|encoding| bytes.starts_with(encoding.bom()))
    }

    pub(crate) fn decode_auto(bytes: Vec<u8>, previous: Option<Self>) -> io::Result<(String, Self)> {
        // An explicit legacy decoder stays in use on watcher reads when the file has no BOM.
        let previous = previous.filter(|encoding| matches!(encoding, Self::Latin1 | Self::Windows1252) || encoding.is_unmarked_utf16());
        if let Some(encoding) = Self::detect_bom(&bytes).or(previous) {
            match encoding.decode(&bytes) {
                Ok(text) => return Ok((text, encoding)),
                Err(err) if encoding != Self::Utf8Bom => return Err(err),
                Err(_) => return Ok((Self::Windows1252.decode(&bytes)?, Self::Windows1252)),
            }
        }
        match String::from_utf8(bytes) {
            Ok(text) => Ok((text, Self::Utf8)),
            Err(err) => Ok((Self::Windows1252.decode(err.as_bytes())?, Self::Windows1252)),
        }
    }

    /// Keep the BOM actually present on disk when the user chooses a decoder.
    pub(crate) fn for_reopen(self, bytes: &[u8]) -> Self {
        match self {
            Self::Utf8 | Self::Utf8Bom => {
                if bytes.starts_with(Self::Utf8Bom.bom()) {
                    Self::Utf8Bom
                } else {
                    Self::Utf8
                }
            }
            Self::Utf16Le if !bytes.starts_with(self.bom()) => Self::Utf16LeNoBom,
            Self::Utf16Be if !bytes.starts_with(self.bom()) => Self::Utf16BeNoBom,
            _ => self,
        }
    }

    pub(crate) fn is_utf16(self) -> bool {
        matches!(self, Self::Utf16Le | Self::Utf16Be) || self.is_unmarked_utf16()
    }

    pub(crate) fn is_unmarked_utf16(self) -> bool {
        matches!(self, Self::Utf16LeNoBom | Self::Utf16BeNoBom)
    }

    pub(crate) fn decode(self, bytes: &[u8]) -> io::Result<String> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, format!("Invalid {} text", self.label()));
        match self {
            Self::Utf8 | Self::Utf8Bom => {
                let bytes = bytes.strip_prefix(Self::Utf8Bom.bom()).unwrap_or(bytes);
                std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| invalid())
            }
            Self::Utf16Le | Self::Utf16Be | Self::Utf16LeNoBom | Self::Utf16BeNoBom => {
                let bytes = bytes.strip_prefix(self.bom()).unwrap_or(bytes);
                let (units, remainder) = bytes.as_chunks::<2>();
                if !remainder.is_empty() {
                    return Err(invalid());
                }
                let from_bytes = if matches!(self, Self::Utf16Le | Self::Utf16LeNoBom) {
                    u16::from_le_bytes
                } else {
                    u16::from_be_bytes
                };
                char::decode_utf16(units.iter().copied().map(from_bytes))
                    .collect::<Result<String, _>>()
                    .map_err(|_| invalid())
            }
            Self::Windows1252 | Self::Latin1 => Ok(bytes
                .iter()
                .map(|&byte| match byte {
                    0x80..=0x9f if self == Self::Windows1252 => WINDOWS_1252[usize::from(byte - 0x80)],
                    _ => char::from(byte),
                })
                .collect()),
        }
    }

    pub(crate) fn encode(self, text: &str) -> io::Result<Vec<u8>> {
        let mut bytes = self.bom().to_vec();
        match self {
            Self::Utf8 | Self::Utf8Bom => bytes.extend_from_slice(text.as_bytes()),
            Self::Utf16Le | Self::Utf16Be | Self::Utf16LeNoBom | Self::Utf16BeNoBom => {
                let to_bytes = if matches!(self, Self::Utf16Le | Self::Utf16LeNoBom) {
                    u16::to_le_bytes
                } else {
                    u16::to_be_bytes
                };
                bytes.extend(text.encode_utf16().flat_map(to_bytes));
            }
            Self::Latin1 | Self::Windows1252 => {
                bytes.reserve(text.len());
                for ch in text.chars() {
                    let byte = match (self, u8::try_from(u32::from(ch)).ok()) {
                        (Self::Windows1252, Some(0x80..=0x9f) | None) => {
                            WINDOWS_1252.iter().position(|&mapped| mapped == ch).map(|i| 0x80 + i as u8)
                        }
                        (_, byte) => byte,
                    };
                    bytes.push(
                        byte.ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, format!("{} cannot represent {ch:?}", self.label()))
                        })?,
                    );
                }
            }
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_encodings_round_trip_with_expected_boms_and_endianness() {
        for (encoding, bytes) in [
            (Encoding::Utf8, &b"A\xf0\x9f\x98\x80"[..]),
            (Encoding::Utf8Bom, &b"\xef\xbb\xbfA\xf0\x9f\x98\x80"[..]),
            (Encoding::Utf16Le, &b"\xff\xfeA\x00\x3d\xd8\x00\xde"[..]),
            (Encoding::Utf16Be, &b"\xfe\xff\x00A\xd8\x3d\xde\x00"[..]),
        ] {
            assert_eq!(Encoding::decode_auto(bytes.to_vec(), None).unwrap(), ("A😀".into(), encoding));
            assert_eq!(encoding.decode(bytes).unwrap(), "A😀");
            assert_eq!(encoding.encode("A😀").unwrap(), bytes);
        }
    }

    #[test]
    fn legacy_encodings_preserve_bytes_and_reject_unrepresentable_text() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(Encoding::decode_auto(bytes.clone(), None).unwrap().1, Encoding::Windows1252);
        assert_eq!(Encoding::Windows1252.decode(b"\x80\xe9").unwrap(), "€é");
        assert_eq!(Encoding::Latin1.decode(b"\x80\xe9").unwrap(), "\u{80}é");
        for encoding in [Encoding::Windows1252, Encoding::Latin1] {
            assert_eq!(encoding.encode(&encoding.decode(&bytes).unwrap()).unwrap(), bytes);
            assert!(encoding.encode("😀").is_err());
        }
        assert!(Encoding::Utf16Be.decode(&[0]).is_err());
        assert!(Encoding::Utf16Le.decode(&[0, 0xd8]).is_err());
    }
}
