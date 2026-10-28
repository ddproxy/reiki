//! Finds financial identifiers in text.
//!
//! Structured identifiers are checked for shape and checksum. Identifiers that have no checksum,
//! such as SWIFT codes and account numbers, are only reported next to a label, an adjacent IBAN
//! or a table header. A hit never carries the matched text, only its position.

use reiki_detect::{digit_run, luhn, word_at, word_before};
use sha2::{Digest, Sha256};
use sha3::Keccak256;

/// What a hit is an identifier of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Iban,
    Swift,
    Crypto,
    Cusip,
    Isin,
    PanSad,
    Account,
    Sedol,
}

impl Kind {
    /// Kinds that are on unless the config narrows the list.
    pub const DEFAULT: [Kind; 6] = [
        Kind::Iban,
        Kind::Swift,
        Kind::Crypto,
        Kind::Cusip,
        Kind::Isin,
        Kind::PanSad,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Iban => "iban",
            Kind::Swift => "swift",
            Kind::Crypto => "crypto",
            Kind::Cusip => "cusip",
            Kind::Isin => "isin",
            Kind::PanSad => "pan_sad",
            Kind::Account => "account",
            Kind::Sedol => "sedol",
        }
    }

    pub fn parse(label: &str) -> Option<Kind> {
        [
            Kind::Iban,
            Kind::Swift,
            Kind::Crypto,
            Kind::Cusip,
            Kind::Isin,
            Kind::PanSad,
            Kind::Account,
            Kind::Sedol,
        ]
        .into_iter()
        .find(|k| k.label() == label)
    }
}

/// A byte range of the scanned text that looks like an identifier.
pub type Hit = reiki_detect::Hit<Kind>;

/// Every hit for the wanted `kinds`, in order of position within each kind.
pub fn scan(text: &str, kinds: &[Kind]) -> Vec<Hit> {
    let bytes = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let lower = lower.as_bytes();
    let mut hits = Vec::new();
    let want = |k: Kind| kinds.contains(&k);

    let mut ibans = Vec::new();
    if want(Kind::Iban) || want(Kind::Swift) {
        iban(bytes, &mut ibans);
    }
    if want(Kind::Iban) {
        hits.extend(ibans.iter().copied());
    }
    if want(Kind::Swift) {
        swift(bytes, lower, &ibans, &mut hits);
    }
    if want(Kind::Crypto) {
        crypto(bytes, &mut hits);
    }
    if want(Kind::Cusip) {
        cusip(bytes, &mut hits);
    }
    if want(Kind::Isin) {
        isin(bytes, &mut hits);
    }
    if want(Kind::PanSad) {
        pan_sad(bytes, lower, &mut hits);
    }
    if want(Kind::Account) {
        keyed(
            bytes,
            lower,
            ACCOUNT_WORDS,
            Kind::Account,
            account_value,
            &mut hits,
        );
    }
    if want(Kind::Sedol) {
        keyed(
            bytes,
            lower,
            SEDOL_WORDS,
            Kind::Sedol,
            sedol_value,
            &mut hits,
        );
    }
    for hit in crate::columnar::scan_tables(text, kinds) {
        if !hits.iter().any(|h| h.start < hit.end && hit.start < h.end) {
            hits.push(hit);
        }
    }
    hits
}

const SWIFT_WORDS: &[&str] = &[
    "swift",
    "bic",
    "swift code",
    "bic code",
    "bank identifier code",
];
const ACCOUNT_WORDS: &[&str] = &[
    "account",
    "account number",
    "account_number",
    "account no",
    "account_no",
    "account #",
    "acct",
    "acct_no",
    "beneficiary",
];
const SEDOL_WORDS: &[&str] = &["sedol", "sedol code", "sedol number"];
const CVV_WORDS: &[&str] = &["cvv", "cvc", "cid", "cvv2", "cvc2", "cav2", "cvn", "cvn2"];

/// True when one of `words` occurs as a whole token in `slice`, offset by `base`.
fn token_in(lower: &[u8], slice: &[u8], words: &[&str], base: usize) -> bool {
    words.iter().any(|w| {
        let w = w.as_bytes();
        let mut p = 0;
        while let Some(q) = slice[p..].windows(w.len()).position(|c| c == w) {
            let s = base + p + q;
            let e = s + w.len();
            if !word_before(lower, s) && !word_at(lower, e) {
                return true;
            }
            p += q + 1;
        }
        false
    })
}

/// True when one of `words` ends as a whole token within `window` bytes before `at`.
fn label_before(lower: &[u8], at: usize, words: &[&str], window: usize) -> bool {
    let at = at.min(lower.len());
    let from = at.saturating_sub(window);
    token_in(lower, &lower[from..at], words, from)
}

/// True when one of `words` starts as a whole token within `window` bytes after `at`.
fn label_after(lower: &[u8], at: usize, words: &[&str], window: usize) -> bool {
    let at = at.min(lower.len());
    let to = (at + window).min(lower.len());
    token_in(lower, &lower[at..to], words, at)
}

/// The longest registry-country IBAN length, keyed by country.
const IBAN_COUNTRIES: &[(&str, usize)] = &[
    ("AL", 28),
    ("AD", 24),
    ("AT", 20),
    ("AZ", 28),
    ("BH", 22),
    ("BY", 28),
    ("BE", 16),
    ("BA", 20),
    ("BR", 29),
    ("BG", 22),
    ("BI", 27),
    ("CR", 22),
    ("HR", 21),
    ("CY", 28),
    ("CZ", 24),
    ("DK", 18),
    ("DJ", 27),
    ("DO", 28),
    ("TL", 23),
    ("EG", 29),
    ("SV", 28),
    ("EE", 20),
    ("FK", 18),
    ("FO", 18),
    ("FI", 18),
    ("FR", 27),
    ("GE", 22),
    ("DE", 22),
    ("GI", 23),
    ("GR", 27),
    ("GL", 18),
    ("GT", 28),
    ("HN", 28),
    ("HU", 28),
    ("IS", 26),
    ("IQ", 23),
    ("IE", 22),
    ("IL", 23),
    ("IT", 27),
    ("JO", 30),
    ("KZ", 20),
    ("XK", 20),
    ("KW", 30),
    ("LV", 21),
    ("LB", 28),
    ("LY", 25),
    ("LI", 21),
    ("LT", 20),
    ("LU", 20),
    ("MT", 31),
    ("MR", 27),
    ("MU", 30),
    ("MC", 27),
    ("MD", 24),
    ("MN", 20),
    ("ME", 22),
    ("NL", 18),
    ("NI", 28),
    ("MK", 19),
    ("NO", 15),
    ("OM", 23),
    ("PK", 24),
    ("PS", 29),
    ("PL", 28),
    ("PT", 25),
    ("QA", 29),
    ("RO", 24),
    ("RU", 33),
    ("LC", 32),
    ("SM", 27),
    ("ST", 25),
    ("SA", 24),
    ("RS", 22),
    ("SC", 31),
    ("SK", 24),
    ("SI", 19),
    ("SO", 23),
    ("ES", 24),
    ("SD", 18),
    ("SE", 24),
    ("CH", 21),
    ("TN", 24),
    ("TR", 26),
    ("UA", 29),
    ("AE", 23),
    ("GB", 22),
    ("VA", 22),
    ("VG", 24),
    ("YE", 30),
];

fn country_len(country: &[u8; 2]) -> Option<usize> {
    IBAN_COUNTRIES
        .iter()
        .find(|(c, _)| c.as_bytes() == country)
        .map(|(_, n)| *n)
}

/// ISO 3166-1 alpha-2 codes used by SWIFT and ISIN.
const ISO_COUNTRIES: &[&str] = &[
    "AD", "AE", "AF", "AG", "AI", "AL", "AM", "AO", "AQ", "AR", "AS", "AT", "AU", "AW", "AX", "AZ",
    "BA", "BB", "BD", "BE", "BF", "BG", "BH", "BI", "BJ", "BL", "BM", "BN", "BO", "BQ", "BR", "BS",
    "BT", "BV", "BW", "BY", "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI", "CK", "CL", "CM", "CN",
    "CO", "CR", "CU", "CV", "CW", "CX", "CY", "CZ", "DE", "DJ", "DK", "DM", "DO", "DZ", "EC", "EE",
    "EG", "EH", "ER", "ES", "ET", "FI", "FJ", "FK", "FM", "FO", "FR", "GA", "GB", "GD", "GE", "GF",
    "GG", "GH", "GI", "GL", "GM", "GN", "GP", "GQ", "GR", "GS", "GT", "GU", "GW", "GY", "HK", "HM",
    "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IM", "IN", "IO", "IQ", "IR", "IS", "IT", "JE", "JM",
    "JO", "JP", "KE", "KG", "KH", "KI", "KM", "KN", "KP", "KR", "KW", "KY", "KZ", "LA", "LB", "LC",
    "LI", "LK", "LR", "LS", "LT", "LU", "LV", "LY", "MA", "MC", "MD", "ME", "MF", "MG", "MH", "MK",
    "ML", "MM", "MN", "MO", "MP", "MQ", "MR", "MS", "MT", "MU", "MV", "MW", "MX", "MY", "MZ", "NA",
    "NC", "NE", "NF", "NG", "NI", "NL", "NO", "NP", "NR", "NU", "NZ", "OM", "PA", "PE", "PF", "PG",
    "PH", "PK", "PL", "PM", "PN", "PR", "PS", "PT", "PW", "PY", "QA", "RE", "RO", "RS", "RU", "RW",
    "SA", "SB", "SC", "SD", "SE", "SG", "SH", "SI", "SJ", "SK", "SL", "SM", "SN", "SO", "SR", "SS",
    "ST", "SV", "SX", "SY", "SZ", "TC", "TD", "TF", "TG", "TH", "TJ", "TK", "TL", "TM", "TN", "TO",
    "TR", "TT", "TV", "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ", "VA", "VC", "VE", "VG", "VI",
    "VN", "VU", "WF", "WS", "YE", "YT", "ZA", "ZM", "ZW",
];

fn is_iso_country(country: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(country) else {
        return false;
    };
    ISO_COUNTRIES.contains(&s.to_ascii_uppercase().as_str())
}

fn mod97(norm: &[u8]) -> u32 {
    let mut rem = 0u32;
    for &b in norm[4..].iter().chain(norm[..4].iter()) {
        if b.is_ascii_digit() {
            rem = (rem * 10 + u32::from(b - b'0')) % 97;
        } else {
            let v = u32::from(b - b'A') + 10;
            rem = (rem * 10 + v / 10) % 97;
            rem = (rem * 10 + v % 10) % 97;
        }
    }
    rem
}

/// The end of a registry-length IBAN at `i`, spaces between groups allowed.
fn iban_at(bytes: &[u8], i: usize, len: usize) -> Option<usize> {
    let mut norm = Vec::with_capacity(len);
    norm.push(bytes[i].to_ascii_uppercase());
    norm.push(bytes[i + 1].to_ascii_uppercase());
    norm.push(bytes[i + 2]);
    norm.push(bytes[i + 3]);
    let mut j = i + 4;
    while norm.len() < len {
        let b = *bytes.get(j)?;
        if b.is_ascii_alphanumeric() {
            norm.push(b.to_ascii_uppercase());
            j += 1;
        } else if b == b' ' && bytes.get(j + 1).is_some_and(u8::is_ascii_alphanumeric) {
            j += 1;
        } else {
            return None;
        }
    }
    (!word_at(bytes, j) && mod97(&norm) == 1).then_some(j)
}

fn iban(bytes: &[u8], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if word_before(bytes, i)
            || !bytes[i].is_ascii_alphabetic()
            || !bytes[i + 1].is_ascii_alphabetic()
            || !bytes[i + 2].is_ascii_digit()
            || !bytes[i + 3].is_ascii_digit()
        {
            i += 1;
            continue;
        }
        let country = [
            bytes[i].to_ascii_uppercase(),
            bytes[i + 1].to_ascii_uppercase(),
        ];
        let Some(len) = country_len(&country) else {
            i += 1;
            continue;
        };
        match iban_at(bytes, i, len) {
            Some(end) => {
                hits.push(Hit {
                    kind: Kind::Iban,
                    start: i,
                    end,
                });
                i = end;
            }
            None => i += 1,
        }
    }
}

/// True when the whole of `raw` is an IBAN, with spaces between groups allowed.
pub(crate) fn valid_iban(raw: &[u8]) -> bool {
    let mut norm = Vec::with_capacity(raw.len());
    for &b in raw {
        if b == b' ' {
            continue;
        }
        if !b.is_ascii_alphanumeric() {
            return false;
        }
        norm.push(b.to_ascii_uppercase());
    }
    if !(15..=33).contains(&norm.len()) {
        return false;
    }
    let country = [norm[0], norm[1]];
    country_len(&country) == Some(norm.len()) && mod97(&norm) == 1
}

fn swift_shape(v: &[u8]) -> bool {
    if v.len() != 8 && v.len() != 11 {
        return false;
    }
    if !v[..6].iter().all(u8::is_ascii_uppercase) {
        return false;
    }
    is_iso_country(&v[4..6])
        && v[6..8].iter().all(u8::is_ascii_alphanumeric)
        && v[8..].iter().all(u8::is_ascii_alphanumeric)
}

fn adjacent_iban(ibans: &[Hit], start: usize, end: usize) -> bool {
    ibans
        .iter()
        .any(|h| (h.start >= end && h.start - end <= 64) || (start >= h.end && start - h.end <= 64))
}

fn swift(bytes: &[u8], lower: &[u8], ibans: &[Hit], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i + 8 <= bytes.len() {
        if word_before(bytes, i) || !bytes[i..i + 6].iter().all(u8::is_ascii_uppercase) {
            i += 1;
            continue;
        }
        let len = if word_at(bytes, i + 8) {
            if i + 11 <= bytes.len()
                && bytes[i + 8..i + 11].iter().all(u8::is_ascii_alphanumeric)
                && !word_at(bytes, i + 11)
            {
                11
            } else {
                i += 1;
                continue;
            }
        } else {
            8
        };
        let end = i + len;
        if swift_shape(&bytes[i..end])
            && (label_before(lower, i, SWIFT_WORDS, 32) || adjacent_iban(ibans, i, end))
        {
            hits.push(Hit {
                kind: Kind::Swift,
                start: i,
                end,
            });
            i = end;
        } else {
            i += 1;
        }
    }
}

pub(crate) fn valid_swift(v: &[u8]) -> bool {
    swift_shape(v)
}

const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn base58_decode(s: &[u8]) -> Option<Vec<u8>> {
    let mut be: Vec<u8> = Vec::new();
    for &c in s {
        let val = BASE58.iter().position(|&a| a == c)? as u32;
        let mut carry = val;
        for b in be.iter_mut().rev() {
            let v = u32::from(*b) * 58 + carry;
            *b = (v & 0xff) as u8;
            carry = v >> 8;
        }
        while carry > 0 {
            be.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let zeros = s.iter().take_while(|&&c| c == b'1').count();
    let mut out = vec![0u8; zeros];
    out.extend_from_slice(&be);
    Some(out)
}

pub(crate) fn valid_base58(s: &[u8]) -> bool {
    if !(26..=35).contains(&s.len()) || !matches!(s[0], b'1' | b'3') {
        return false;
    }
    let Some(raw) = base58_decode(s) else {
        return false;
    };
    if raw.len() != 25 || !matches!(raw[0], 0 | 5) {
        return false;
    }
    if (s[0] == b'1') != (raw[0] == 0) {
        return false;
    }
    let check = Sha256::digest(Sha256::digest(&raw[..21]));
    check[..4] == raw[21..25]
}

const BECH32: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn polymod(values: impl Iterator<Item = u8>) -> u32 {
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut chk = 1u32;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x1ffffff) << 5) ^ u32::from(v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.iter().map(|b| b >> 5).collect();
    out.push(0);
    out.extend(hrp.iter().map(|b| b & 31));
    out
}

fn convert_bits(data: &[u8], from: u32, to: u32) -> Option<Vec<u8>> {
    let maxv = (1u32 << to) - 1;
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for &v in data {
        if u32::from(v) >> from != 0 {
            return None;
        }
        acc = (acc << from) | u32::from(v);
        bits += from;
        while bits >= to {
            bits -= to;
            out.push(((acc >> bits) & maxv) as u8);
        }
    }
    if bits >= from || (acc << (to - bits)) & maxv != 0 {
        return None;
    }
    Some(out)
}

pub(crate) fn valid_bech32(s: &[u8]) -> bool {
    let has_lower = s.iter().any(u8::is_ascii_lowercase);
    let has_upper = s.iter().any(u8::is_ascii_uppercase);
    if has_lower && has_upper {
        return false;
    }
    let lower: Vec<u8> = s.iter().map(|b| b.to_ascii_lowercase()).collect();
    if lower.len() < 11 || lower[2] != b'1' {
        return false;
    }
    let hrp = &lower[..2];
    if hrp != b"bc" && hrp != b"tb" {
        return false;
    }
    let data = &lower[3..];
    if data.len() < 7 {
        return false;
    }
    let mut vals = Vec::with_capacity(data.len());
    for &c in data {
        let Some(v) = BECH32.iter().position(|&a| a == c) else {
            return false;
        };
        vals.push(v as u8);
    }
    let version = vals[0];
    if version > 16 {
        return false;
    }
    let constant = if version == 0 { 1 } else { 0x2bc8_30a3 };
    if polymod(hrp_expand(hrp).into_iter().chain(vals.iter().copied())) != constant {
        return false;
    }
    let Some(program) = convert_bits(&vals[1..vals.len() - 6], 5, 8) else {
        return false;
    };
    if version == 0 {
        program.len() == 20 || program.len() == 32
    } else {
        (2..=40).contains(&program.len())
    }
}

pub(crate) fn valid_eth(hex: &[u8]) -> bool {
    if hex.len() != 40 || !hex.iter().all(u8::is_ascii_hexdigit) {
        return false;
    }
    let has_lower = hex.iter().any(u8::is_ascii_lowercase);
    let has_upper = hex.iter().any(u8::is_ascii_uppercase);
    if !(has_lower && has_upper) {
        return true;
    }
    let lower: Vec<u8> = hex.iter().map(|b| b.to_ascii_lowercase()).collect();
    let hash = Keccak256::digest(&lower);
    for (i, &c) in hex.iter().enumerate() {
        if c.is_ascii_alphabetic() {
            let nibble = if i % 2 == 0 {
                hash[i / 2] >> 4
            } else {
                hash[i / 2] & 0x0f
            };
            if c.is_ascii_uppercase() != (nibble >= 8) {
                return false;
            }
        }
    }
    true
}

fn base58(bytes: &[u8], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'1' | b'3') && !word_before(bytes, i) {
            let mut j = i;
            while j < bytes.len() && BASE58.contains(&bytes[j]) && j - i < 36 {
                j += 1;
            }
            if (26..=35).contains(&(j - i)) && !word_at(bytes, j) && valid_base58(&bytes[i..j]) {
                hits.push(Hit {
                    kind: Kind::Crypto,
                    start: i,
                    end: j,
                });
                i = j;
                continue;
            }
        }
        i += 1;
    }
}

fn bech32(bytes: &[u8], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if !word_before(bytes, i)
            && (bytes[i..i + 3].eq_ignore_ascii_case(b"bc1")
                || bytes[i..i + 3].eq_ignore_ascii_case(b"tb1"))
        {
            let mut j = i + 3;
            while j < bytes.len() && bytes[j].is_ascii_alphanumeric() && j - i < 90 {
                j += 1;
            }
            if !word_at(bytes, j) && valid_bech32(&bytes[i..j]) {
                hits.push(Hit {
                    kind: Kind::Crypto,
                    start: i,
                    end: j,
                });
                i = j;
                continue;
            }
        }
        i += 1;
    }
}

fn eth(bytes: &[u8], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i + 42 <= bytes.len() {
        if !word_before(bytes, i)
            && bytes[i] == b'0'
            && matches!(bytes[i + 1], b'x' | b'X')
            && valid_eth(&bytes[i + 2..i + 42])
            && !word_at(bytes, i + 42)
        {
            hits.push(Hit {
                kind: Kind::Crypto,
                start: i,
                end: i + 42,
            });
            i += 42;
        } else {
            i += 1;
        }
    }
}

fn crypto(bytes: &[u8], hits: &mut Vec<Hit>) {
    base58(bytes, hits);
    bech32(bytes, hits);
    eth(bytes, hits);
}

pub(crate) fn valid_crypto(v: &[u8]) -> bool {
    valid_base58(v)
        || valid_bech32(v)
        || (v.len() == 42 && matches!(v[1], b'x' | b'X') && v[0] == b'0' && valid_eth(&v[2..]))
}

fn char_value(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some(u32::from(c - b'0')),
        b'A'..=b'Z' => Some(u32::from(c - b'A') + 10),
        b'*' => Some(36),
        b'@' => Some(37),
        b'#' => Some(38),
        _ => None,
    }
}

pub(crate) fn valid_cusip(c: &[u8]) -> bool {
    if c.len() != 9 {
        return false;
    }
    let mut sum = 0u32;
    for (i, &ch) in c[..8].iter().enumerate() {
        let Some(v) = char_value(ch) else {
            return false;
        };
        let v = if i % 2 == 1 { v * 2 } else { v };
        sum += v / 10 + v % 10;
    }
    let Some(last) = char_value(c[8]) else {
        return false;
    };
    last < 10 && (10 - sum % 10) % 10 == last
}

fn cusip(bytes: &[u8], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i + 9 <= bytes.len() {
        if !word_before(bytes, i) && !word_at(bytes, i + 9) && valid_cusip(&bytes[i..i + 9]) {
            hits.push(Hit {
                kind: Kind::Cusip,
                start: i,
                end: i + 9,
            });
            i += 9;
        } else {
            i += 1;
        }
    }
}

pub(crate) fn valid_isin(c: &[u8]) -> bool {
    if c.len() != 12
        || !c[0].is_ascii_uppercase()
        || !c[1].is_ascii_uppercase()
        || !c[2..11]
            .iter()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
        || !c[11].is_ascii_digit()
    {
        return false;
    }
    let country = [c[0], c[1]];
    let Ok(country) = std::str::from_utf8(&country) else {
        return false;
    };
    if !ISO_COUNTRIES.contains(&country) && !matches!(country, "XS" | "EU" | "EZ" | "UN") {
        return false;
    }
    let mut digits = Vec::with_capacity(24);
    for &b in &c[..11] {
        if b.is_ascii_digit() {
            digits.push(b);
        } else {
            let v = b - b'A' + 10;
            digits.push(b'0' + v / 10);
            digits.push(b'0' + v % 10);
        }
    }
    digits.push(c[11]);
    luhn(&digits)
}

fn isin(bytes: &[u8], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i + 12 <= bytes.len() {
        if !word_before(bytes, i) && !word_at(bytes, i + 12) && valid_isin(&bytes[i..i + 12]) {
            hits.push(Hit {
                kind: Kind::Isin,
                start: i,
                end: i + 12,
            });
            i += 12;
        } else {
            i += 1;
        }
    }
}

/// Card numbers with a known leading digit and a valid Luhn checksum.
fn card_spans(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() || word_before(bytes, i) {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut digits = Vec::with_capacity(19);
        loop {
            let n = digit_run(bytes, j);
            digits.extend_from_slice(&bytes[j..j + n]);
            j += n;
            let more = matches!(bytes.get(j), Some(b'-' | b' '))
                && bytes.get(j + 1).is_some_and(u8::is_ascii_digit);
            if !more || digits.len() >= 19 {
                break;
            }
            j += 1;
        }
        if (13..=19).contains(&digits.len())
            && (b'2'..=b'6').contains(&digits[0])
            && !word_at(bytes, j)
            && luhn(&digits)
        {
            spans.push((i, j));
            i = j;
        } else {
            i += digit_run(bytes, i).max(1);
        }
    }
    spans
}

fn pan_sad(bytes: &[u8], lower: &[u8], hits: &mut Vec<Hit>) {
    // Track data starts with %B and carries the PAN first.
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'%' && bytes[i + 1] == b'B' && !word_before(bytes, i) {
            let n = digit_run(bytes, i + 2);
            if (13..=19).contains(&n) {
                let digits = &bytes[i + 2..i + 2 + n];
                if (b'2'..=b'6').contains(&digits[0]) && !word_at(bytes, i + 2 + n) && luhn(digits)
                {
                    hits.push(Hit {
                        kind: Kind::PanSad,
                        start: i + 2,
                        end: i + 2 + n,
                    });
                }
            }
        }
        i += 1;
    }
    for (start, end) in card_spans(bytes) {
        if hits.iter().any(|h| h.start < end && start < h.end) {
            continue;
        }
        if label_before(lower, start, CVV_WORDS, 64) || label_after(lower, end, CVV_WORDS, 64) {
            hits.push(Hit {
                kind: Kind::PanSad,
                start,
                end,
            });
        }
    }
}

fn account_value(bytes: &[u8], at: usize) -> Option<usize> {
    let n = digit_run(bytes, at);
    ((6..=17).contains(&n) && !word_at(bytes, at + n)).then_some(at + n)
}

pub(crate) fn valid_account(v: &[u8]) -> bool {
    (6..=17).contains(&v.len()) && v.iter().all(u8::is_ascii_digit)
}

pub(crate) fn valid_sedol(s: &[u8]) -> bool {
    if s.len() != 7 {
        return false;
    }
    const WEIGHTS: [u32; 7] = [1, 3, 1, 7, 3, 9, 1];
    let mut sum = 0u32;
    for (i, &c) in s.iter().enumerate() {
        let Some(v) = char_value(c) else {
            return false;
        };
        if v > 35 {
            return false;
        }
        sum += v * WEIGHTS[i];
    }
    sum.is_multiple_of(10)
}

fn sedol_value(bytes: &[u8], at: usize) -> Option<usize> {
    let n = bytes[at..]
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric())
        .count();
    (n == 7 && !word_at(bytes, at + n) && valid_sedol(&bytes[at..at + n])).then_some(at + n)
}

/// Finds each keyword, then asks `value` for the end of the identifier that follows it.
fn keyed(
    bytes: &[u8],
    lower: &[u8],
    words: &[&str],
    kind: Kind,
    value: fn(&[u8], usize) -> Option<usize>,
    hits: &mut Vec<Hit>,
) {
    for w in words {
        let w = w.as_bytes();
        let mut from = 0;
        while let Some(at) = lower[from..]
            .windows(w.len())
            .position(|c| c == w)
            .map(|p| p + from)
        {
            from = at + w.len();
            if word_before(lower, at) || word_at(lower, from) {
                continue;
            }
            let mut start = from;
            let skipped = bytes[start..]
                .iter()
                .take(8)
                .take_while(|b| matches!(**b, b' ' | b':' | b'#' | b'=' | b'"' | b'\'' | b'.'))
                .count();
            start += skipped;
            if let Some(end) = value(bytes, start) {
                hits.push(Hit { kind, start, end });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Kind> {
        let mut k = Kind::DEFAULT.to_vec();
        k.extend([Kind::Account, Kind::Sedol]);
        k
    }

    fn kinds(text: &str) -> Vec<Kind> {
        let mut found: Vec<Kind> = scan(text, &all()).into_iter().map(|h| h.kind).collect();
        found.sort();
        found
    }

    #[test]
    fn iban_needs_a_registry_length_and_a_valid_mod_97() {
        assert_eq!(kinds("send GB82WEST12345698765432 now"), [Kind::Iban]);
        assert_eq!(kinds("send GB82 WEST 1234 5698 7654 32 now"), [Kind::Iban]);
        assert_eq!(kinds("send gb82west12345698765432 now"), [Kind::Iban]);
        assert!(
            kinds("GB82WEST12345698765433").is_empty(),
            "bad check digits"
        );
        assert!(kinds("GB82-WEST-12345698765432").is_empty(), "punctuation");
        assert!(kinds("xGB82WEST12345698765432").is_empty(), "inside a word");
        assert!(
            kinds("ZZ82WEST12345698765432").is_empty(),
            "unknown country"
        );
    }

    #[test]
    fn swift_needs_a_label_or_an_adjacent_iban() {
        assert_eq!(kinds("BIC DEUTDEFF500"), [Kind::Swift]);
        assert_eq!(kinds("swift: CHASUS33"), [Kind::Swift]);
        assert_eq!(
            kinds("iban GB82WEST12345698765432 bic DEUTDEFF"),
            [Kind::Iban, Kind::Swift]
        );
        assert!(kinds("DEUTDEFF500").is_empty(), "bare BIC");
        assert!(kinds("BIC DEUTD3FF").is_empty(), "digit in country");
        assert!(kinds("BIC deutdeff").is_empty(), "lowercase");
        assert!(kinds("bicycle DEUTDEFF").is_empty(), "label inside a word");
    }

    #[test]
    fn bitcoin_and_ethereum_addresses_are_checked() {
        assert_eq!(
            kinds("pay 1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa"),
            [Kind::Crypto]
        );
        assert_eq!(
            kinds("pay bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"),
            [Kind::Crypto]
        );
        assert_eq!(
            kinds("pay 0x52908400098527886E0F7030069857D2E4169EE7"),
            [Kind::Crypto]
        );
        assert_eq!(
            kinds("pay 0xde709f2102306220921060314715629080e2fb77"),
            [Kind::Crypto],
            "all lowercase is unchecksummed but valid"
        );
        assert!(
            kinds("1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNb").is_empty(),
            "base58 checksum"
        );
        assert!(
            kinds("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdp").is_empty(),
            "bech32 checksum"
        );
        assert_eq!(
            kinds("0x52908400098527886E0F7030069857D2E4169EE0"),
            [Kind::Crypto],
            "all-uppercase hex is unchecksummed but valid"
        );
        assert_eq!(
            kinds("0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"),
            [Kind::Crypto]
        );
        assert!(
            kinds("0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAeD").is_empty(),
            "mixed case must follow EIP-55"
        );
        assert!(
            kinds("hash 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .is_empty(),
            "transaction hash is not an address"
        );
        assert!(kinds("x1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa").is_empty());
    }

    #[test]
    fn cusip_and_isin_need_their_checksums() {
        assert_eq!(kinds("037833100"), [Kind::Cusip]);
        assert_eq!(kinds("17275R102"), [Kind::Cusip]);
        assert!(kinds("037833101").is_empty(), "CUSIP check digit");
        assert!(kinds("x037833100").is_empty());

        assert_eq!(kinds("US0378331005"), [Kind::Isin]);
        assert_eq!(kinds("AU0000XVGZA3"), [Kind::Isin]);
        assert_eq!(kinds("GB0002634946"), [Kind::Isin]);
        assert!(kinds("US0378331004").is_empty(), "ISIN check digit");
        assert!(kinds("ZZ0378331005").is_empty(), "unknown country");
        assert!(kinds("US03783310050").is_empty(), "too long");
    }

    #[test]
    fn pan_sad_needs_a_cvv_or_track_data() {
        assert_eq!(kinds("card 4111111111111111 cvv 123"), [Kind::PanSad]);
        assert_eq!(kinds("cvc: 1234\n4111 1111 1111 1111"), [Kind::PanSad]);
        assert_eq!(
            kinds("%B4111111111111111^9901101^123456789012?"),
            [Kind::PanSad]
        );
        assert!(
            kinds("4111111111111111").is_empty(),
            "a bare PAN is not SAD"
        );
        assert!(
            kinds("cvv 123 card 4111111111111112").is_empty(),
            "bad PAN checksum"
        );
        assert!(
            kinds("cvvx 4111111111111111").is_empty(),
            "cvv inside a word"
        );
        assert!(kinds("%B4111111111111112^").is_empty(), "bad track PAN");
    }

    #[test]
    fn account_and_sedol_are_header_or_label_bound() {
        assert_eq!(kinds("account number: 000123456789"), [Kind::Account]);
        assert_eq!(kinds("acct_no 12345678"), [Kind::Account]);
        assert_eq!(kinds("sedol 0263494"), [Kind::Sedol]);
        assert_eq!(kinds("SEDOL: 0540528"), [Kind::Sedol]);
        assert!(kinds("000123456789").is_empty(), "bare account digits");
        assert!(kinds("0263494").is_empty(), "bare SEDOL");
        assert!(kinds("sedol 0263495").is_empty(), "SEDOL check digit");
        assert!(kinds("account number: 123").is_empty());
    }

    #[test]
    fn finance_tables_map_headers_to_kinds() {
        assert_eq!(
            kinds("id,iban,swift\n1,GB82WEST12345698765432,DEUTDEFF\n"),
            [Kind::Iban, Kind::Swift]
        );
        assert_eq!(
            kinds("cusip,isin\n037833100,US0378331005\n"),
            [Kind::Cusip, Kind::Isin]
        );
        assert_eq!(
            kinds("wallet\n1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa\n"),
            [Kind::Crypto]
        );
        assert_eq!(
            kinds("id,account\n1,000123456789\n"),
            [Kind::Account],
            "account is off by default but can be asked for"
        );
        assert!(scan("id,account\n1,000123456789\n", &Kind::DEFAULT).is_empty());
    }

    #[test]
    fn hits_report_positions_and_never_text() {
        let text = "x 037833100 y";
        let hit = scan(text, &all())[0];
        assert_eq!((hit.start, hit.end), (2, 11));
    }

    #[test]
    fn ordinary_prose_and_code_are_clean() {
        let text = "fn main() { let account = 42; let wallet = \"0.0.0\"; } // build 2026-10-28";
        assert!(scan(text, &all()).is_empty());
        assert!(scan("id 123456789 price 19.99", &all()).is_empty());
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for text in [
            "GB",
            "GB82",
            "GB82WEST",
            "%B",
            "%B4111",
            "0x",
            "0x5290",
            "bc1",
            "bc1q",
            "CUSIP",
            "xxxxxxxxxxxx",
            "        ",
            "\u{0}\u{1}\u{fffd}",
        ] {
            let _ = scan(text, &all());
        }
        let lossy = String::from_utf8_lossy(&[0xff, 0xfe, b'G', b'B', 0x80, b'1']);
        let _ = scan(&lossy, &all());
    }
}
