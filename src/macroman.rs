//! MacRoman <-> UTF-8 conversion.
//!
//! MFS stores every name (volume names, file names) as a Pascal string of
//! MacRoman bytes. Bytes `0x00..=0x7F` are plain ASCII; `0x80..=0xFF` map to
//! the 128 characters in [`HIGH`], which is Apple's standard MacRoman table in
//! its modern revision (the one where `0xDB` is `€` rather than the currency
//! sign `¤` used before Mac OS 8.5).

/// MacRoman characters for bytes `0x80..=0xFF`, indexed by `byte - 0x80`.
const HIGH: [char; 128] = [
    // 0x80
    '\u{00C4}', // Ä
    '\u{00C5}', // Å
    '\u{00C7}', // Ç
    '\u{00C9}', // É
    '\u{00D1}', // Ñ
    '\u{00D6}', // Ö
    '\u{00DC}', // Ü
    '\u{00E1}', // á
    '\u{00E0}', // à
    '\u{00E2}', // â
    '\u{00E4}', // ä
    '\u{00E3}', // ã
    '\u{00E5}', // å
    '\u{00E7}', // ç
    '\u{00E9}', // é
    '\u{00E8}', // è
    // 0x90
    '\u{00EA}', // ê
    '\u{00EB}', // ë
    '\u{00ED}', // í
    '\u{00EC}', // ì
    '\u{00EE}', // î
    '\u{00EF}', // ï
    '\u{00F1}', // ñ
    '\u{00F3}', // ó
    '\u{00F2}', // ò
    '\u{00F4}', // ô
    '\u{00F6}', // ö
    '\u{00F5}', // õ
    '\u{00FA}', // ú
    '\u{00F9}', // ù
    '\u{00FB}', // û
    '\u{00FC}', // ü
    // 0xA0
    '\u{2020}', // † dagger
    '\u{00B0}', // °
    '\u{00A2}', // ¢
    '\u{00A3}', // £
    '\u{00A7}', // §
    '\u{2022}', // • bullet
    '\u{00B6}', // ¶
    '\u{00DF}', // ß
    '\u{00AE}', // ®
    '\u{00A9}', // ©
    '\u{2122}', // ™
    '\u{00B4}', // ´ acute accent
    '\u{00A8}', // ¨ diaeresis
    '\u{2260}', // ≠
    '\u{00C6}', // Æ
    '\u{00D8}', // Ø
    // 0xB0
    '\u{221E}', // ∞
    '\u{00B1}', // ±
    '\u{2264}', // ≤
    '\u{2265}', // ≥
    '\u{00A5}', // ¥
    '\u{00B5}', // µ micro sign
    '\u{2202}', // ∂
    '\u{2211}', // ∑ n-ary summation
    '\u{220F}', // ∏ n-ary product
    '\u{03C0}', // π greek small letter pi
    '\u{222B}', // ∫
    '\u{00AA}', // ª
    '\u{00BA}', // º
    '\u{03A9}', // Ω greek capital omega (ohm sign)
    '\u{00E6}', // æ
    '\u{00F8}', // ø
    // 0xC0
    '\u{00BF}', // ¿
    '\u{00A1}', // ¡
    '\u{00AC}', // ¬
    '\u{221A}', // √
    '\u{0192}', // ƒ latin small letter f with hook
    '\u{2248}', // ≈
    '\u{2206}', // ∆ increment
    '\u{00AB}', // «
    '\u{00BB}', // »
    '\u{2026}', // … horizontal ellipsis
    '\u{00A0}', // no-break space
    '\u{00C0}', // À
    '\u{00C3}', // Ã
    '\u{00D5}', // Õ
    '\u{0152}', // Œ
    '\u{0153}', // œ
    // 0xD0
    '\u{2013}', // – en dash
    '\u{2014}', // — em dash
    '\u{201C}', // “
    '\u{201D}', // ”
    '\u{2018}', // ‘
    '\u{2019}', // ’
    '\u{00F7}', // ÷
    '\u{25CA}', // ◊ lozenge
    '\u{00FF}', // ÿ
    '\u{0178}', // Ÿ
    '\u{2044}', // ⁄ fraction slash
    '\u{20AC}', // € euro sign (was ¤ before Mac OS 8.5)
    '\u{2039}', // ‹
    '\u{203A}', // ›
    '\u{FB01}', // ﬁ ligature
    '\u{FB02}', // ﬂ ligature
    // 0xE0
    '\u{2021}', // ‡ double dagger
    '\u{00B7}', // · middle dot
    '\u{201A}', // ‚
    '\u{201E}', // „
    '\u{2030}', // ‰ per mille
    '\u{00C2}', // Â
    '\u{00CA}', // Ê
    '\u{00C1}', // Á
    '\u{00CB}', // Ë
    '\u{00C8}', // È
    '\u{00CD}', // Í
    '\u{00CE}', // Î
    '\u{00CF}', // Ï
    '\u{00CC}', // Ì
    '\u{00D3}', // Ó
    '\u{00D4}', // Ô
    // 0xF0
    '\u{F8FF}', // Apple logo (private use)
    '\u{00D2}', // Ò
    '\u{00DA}', // Ú
    '\u{00DB}', // Û
    '\u{00D9}', // Ù
    '\u{0131}', // ı dotless i
    '\u{02C6}', // ˆ modifier circumflex
    '\u{02DC}', // ˜ small tilde
    '\u{00AF}', // ¯ macron
    '\u{02D8}', // ˘ breve
    '\u{02D9}', // ˙ dot above
    '\u{02DA}', // ˚ ring above
    '\u{00B8}', // ¸ cedilla
    '\u{02DD}', // ˝ double acute
    '\u{02DB}', // ˛ ogonek
    '\u{02C7}', // ˇ caron
];

/// Decode MacRoman bytes into a `String`. Cannot fail: every one of the 256
/// byte values has a Unicode counterpart.
pub(crate) fn decode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                HIGH[(b - 0x80) as usize]
            }
        })
        .collect()
}

/// Encode a `str` as MacRoman bytes.
///
/// Returns `Err(c)` with the first character that has no MacRoman
/// representation.
pub(crate) fn encode(s: &str) -> Result<Vec<u8>, char> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if (c as u32) < 0x80 {
            out.push(c as u8);
        } else {
            match HIGH.iter().position(|&h| h == c) {
                Some(i) => out.push(0x80 + i as u8),
                None => return Err(c),
            }
        }
    }
    Ok(out)
}

/// Case-insensitive comparison of two already-decoded MacRoman strings.
///
/// MFS name lookup is case-insensitive the way the classic Mac Toolbox's
/// `_RelString` was. This is an approximation: we fold both sides with Unicode
/// `to_uppercase()` instead of replaying the ROM's MacRoman uppercase table.
/// The two agree on ASCII and on all the accented Latin letters that appear in
/// practice; they can differ for exotic case pairs (e.g. `ß`/`SS`, or
/// characters whose Unicode uppercase expands to multiple code points, which
/// `_RelString` leaves alone).
pub(crate) fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.to_uppercase() == b.to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_bytes() {
        for b in 0u8..=0xFF {
            let s = decode(&[b]);
            assert_eq!(s.chars().count(), 1, "byte {b:#04x} decoded to {s:?}");
            let back = encode(&s).unwrap_or_else(|c| panic!("byte {b:#04x} -> {c:?} unmappable"));
            assert_eq!(back, vec![b], "byte {b:#04x} did not round-trip");
        }
    }

    #[test]
    fn table_is_a_bijection() {
        let mut sorted = HIGH;
        sorted.sort_unstable();
        for w in sorted.windows(2) {
            assert_ne!(w[0], w[1], "duplicate entry {:?} in MacRoman table", w[0]);
        }
    }

    #[test]
    fn known_pairs() {
        assert_eq!(decode(&[0x80]), "Ä");
        assert_eq!(decode(&[0x8E]), "é");
        assert_eq!(decode(&[0xA5]), "\u{2022}"); // bullet
        assert_eq!(decode(&[0xAA]), "™");
        assert_eq!(decode(&[0xB9]), "π");
        assert_eq!(decode(&[0xC9]), "\u{2026}"); // ellipsis
        assert_eq!(decode(&[0xCA]), "\u{00A0}"); // no-break space
        assert_eq!(decode(&[0xD0]), "\u{2013}"); // en dash
        assert_eq!(decode(&[0xD5]), "\u{2019}"); // right single quote
        assert_eq!(decode(&[0xF0]), "\u{F8FF}"); // Apple logo

        assert_eq!(encode("Ä").unwrap(), vec![0x80]);
        assert_eq!(encode("é").unwrap(), vec![0x8E]);
        assert_eq!(encode("\u{2022}").unwrap(), vec![0xA5]);
    }

    #[test]
    fn ascii_passthrough() {
        assert_eq!(decode(b"System Folder"), "System Folder");
        assert_eq!(encode("System Folder").unwrap(), b"System Folder".to_vec());
        assert_eq!(decode(&[]), "");
        assert_eq!(encode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn mixed_string() {
        let bytes = [b'C', b'a', b'f', 0x8E, b' ', 0xA5, b'!'];
        assert_eq!(decode(&bytes), "Café \u{2022}!");
        assert_eq!(encode("Café \u{2022}!").unwrap(), bytes.to_vec());
    }

    #[test]
    fn encode_rejects_unmappable() {
        assert_eq!(encode("\u{224B}"), Err('\u{224B}')); // ≋
        assert_eq!(encode("你好"), Err('你'));
        // The bad character is reported even when it comes after good ones.
        assert_eq!(encode("Café 你"), Err('你'));
    }

    #[test]
    fn case_insensitive_compare() {
        assert!(eq_ignore_case("readme", "README"));
        assert!(eq_ignore_case("ReadMe", "rEADmE"));
        assert!(eq_ignore_case("café", "CAFÉ"));
        assert!(eq_ignore_case("Ångström", "ÅNGSTRÖM"));
        assert!(eq_ignore_case("", ""));
        assert!(!eq_ignore_case("readme", "read me"));
        assert!(!eq_ignore_case("café", "cafe"));
    }
}
