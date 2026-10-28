//! Finds government markings and identifier values in text.
//!
//! Markings are detected from their own upper-case form, such as a CUI banner or a classification
//! level with a `//` caveat chain. Identifiers with no usable checksum are only reported next to
//! their own label. A hit never carries the matched text, only its position.

use reiki_detect::{digit_run, word_at, word_before};

/// What a hit is an identifier of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Cui,
    Classification,
    Dissemination,
    Ein,
    Itin,
    Cage,
    Uei,
    Edipi,
    Eccn,
}

impl Kind {
    /// Kinds that are on unless the config narrows the list.
    pub const DEFAULT: [Kind; 3] = [Kind::Cui, Kind::Classification, Kind::Dissemination];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Cui => "cui",
            Kind::Classification => "classification",
            Kind::Dissemination => "dissemination",
            Kind::Ein => "ein",
            Kind::Itin => "itin",
            Kind::Cage => "cage",
            Kind::Uei => "uei",
            Kind::Edipi => "edipi",
            Kind::Eccn => "eccn",
        }
    }

    pub fn parse(label: &str) -> Option<Kind> {
        [
            Kind::Cui,
            Kind::Classification,
            Kind::Dissemination,
            Kind::Ein,
            Kind::Itin,
            Kind::Cage,
            Kind::Uei,
            Kind::Edipi,
            Kind::Eccn,
        ]
        .into_iter()
        .find(|k| k.label() == label)
    }
}

/// A byte range of the scanned text that looks like an identifier.
pub type Hit = reiki_detect::Hit<Kind>;

const EIN_WORDS: &[&str] = &[
    "ein",
    "employer identification number",
    "employer identification no",
    "employer id",
];
const ITIN_WORDS: &[&str] = &[
    "itin",
    "individual taxpayer identification number",
    "individual taxpayer identification no",
    "individual taxpayer id",
];
const CAGE_WORDS: &[&str] = &["cage", "cage code", "cage number"];
const UEI_WORDS: &[&str] = &[
    "uei",
    "unique entity id",
    "unique entity identifier",
    "unique entity identification number",
];
const EDIPI_WORDS: &[&str] = &[
    "edipi",
    "dod id",
    "dod_id",
    "dodid",
    "dod identification number",
];
const ECCN_WORDS: &[&str] = &[
    "eccn",
    "export control classification number",
    "export control classification no",
];

/// Every hit for the wanted `kinds`, in order of position within each kind.
pub fn scan(text: &str, kinds: &[Kind]) -> Vec<Hit> {
    let bytes = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let lower = lower.as_bytes();
    let mut hits = Vec::new();
    let want = |k: Kind| kinds.contains(&k);
    if want(Kind::Cui) {
        cui(bytes, &mut hits);
    }
    if want(Kind::Classification) {
        classification(bytes, &mut hits);
    }
    if want(Kind::Dissemination) {
        dissemination(bytes, &mut hits);
    }
    if want(Kind::Ein) {
        keyed(bytes, lower, EIN_WORDS, Kind::Ein, ein_value, &mut hits);
    }
    if want(Kind::Itin) {
        keyed(bytes, lower, ITIN_WORDS, Kind::Itin, itin_value, &mut hits);
    }
    if want(Kind::Cage) {
        keyed(bytes, lower, CAGE_WORDS, Kind::Cage, cage_value, &mut hits);
    }
    if want(Kind::Uei) {
        keyed(bytes, lower, UEI_WORDS, Kind::Uei, uei_value, &mut hits);
    }
    if want(Kind::Edipi) {
        keyed(
            bytes,
            lower,
            EDIPI_WORDS,
            Kind::Edipi,
            edipi_value,
            &mut hits,
        );
    }
    if want(Kind::Eccn) {
        keyed(bytes, lower, ECCN_WORDS, Kind::Eccn, eccn_value, &mut hits);
    }
    hits
}

/// A stand-alone `CUI` banner or a `CUI//SP-...` marker.
fn cui(bytes: &[u8], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if word_before(bytes, i) || &bytes[i..i + 3] != b"CUI" || word_at(bytes, i + 3) {
            i += 1;
            continue;
        }
        let mut end = i + 3;
        if bytes[i + 3..].starts_with(b"//SP-") {
            let mut j = i + 6;
            let start = j;
            while j < bytes.len()
                && (bytes[j].is_ascii_uppercase() || bytes[j].is_ascii_digit() || bytes[j] == b'-')
            {
                j += 1;
            }
            let code = &bytes[start..j];
            if (2..=8).contains(&code.len()) && !code.ends_with(b"-") && !word_at(bytes, j) {
                end = j;
            }
        }
        hits.push(Hit {
            kind: Kind::Cui,
            start: i,
            end,
        });
        i = end;
    }
}

const LEVELS: [&[u8]; 3] = [b"TOP SECRET", b"SECRET", b"CONFIDENTIAL"];

/// A classification level followed by a `//` caveat chain.
fn classification(bytes: &[u8], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i < bytes.len() {
        let mut matched = None;
        for level in LEVELS {
            let end = i + level.len();
            if end <= bytes.len()
                && !word_before(bytes, i)
                && !word_at(bytes, end)
                && bytes[i..end] == *level
                && bytes[end..].starts_with(b"//")
            {
                if let Some(banner) = banner_end(bytes, end) {
                    matched = Some(banner);
                    break;
                }
            }
        }
        match matched {
            Some(end) => {
                hits.push(Hit {
                    kind: Kind::Classification,
                    start: i,
                    end,
                });
                i = end;
            }
            None => i += 1,
        }
    }
}

/// The end of a caveat chain that starts at the first `//`, or `None` when no segment parses.
fn banner_end(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut end = None;
    let mut length = 0;
    while bytes[i..].starts_with(b"//") {
        let start = i + 2;
        let Some(next) = segment(bytes, start) else {
            break;
        };
        length += next - start;
        if length > 96 {
            break;
        }
        i = next;
        end = Some(i);
    }
    end
}

/// One caveat segment: `REL TO <list>`, `FIVE EYES`, `FVEY` or an upper-case token.
fn segment(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes[i..].starts_with(b"REL TO ") {
        return release_list(bytes, i + 7);
    }
    if bytes[i..].starts_with(b"FIVE EYES") && !word_at(bytes, i + 9) {
        return Some(i + 9);
    }
    if bytes[i..].starts_with(b"FVEY") && !word_at(bytes, i + 4) {
        return Some(i + 4);
    }
    let start = i;
    let mut j = i;
    while j < bytes.len()
        && (bytes[j].is_ascii_uppercase() || bytes[j].is_ascii_digit() || bytes[j] == b'-')
    {
        j += 1;
    }
    let token = &bytes[start..j];
    (!token.is_empty() && token.len() <= 16 && !token.ends_with(b"-") && !word_at(bytes, j))
        .then_some(j)
}

/// A comma-separated release list such as `USA, GBR` or `FIVE EYES`.
fn release_list(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut count = 0;
    loop {
        if bytes[i..].starts_with(b"FIVE EYES") && !word_at(bytes, i + 9) {
            i += 9;
        } else if bytes[i..].starts_with(b"FVEY") && !word_at(bytes, i + 4) {
            i += 4;
        } else {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_uppercase() {
                i += 1;
            }
            if !(2..=4).contains(&(i - start)) || word_at(bytes, i) {
                return None;
            }
        }
        count += 1;
        if count >= 16 || bytes.get(i) != Some(&b',') {
            return Some(i);
        }
        i += 1;
        while bytes.get(i) == Some(&b' ') {
            i += 1;
        }
    }
}

/// Stand-alone `NOFORN` and `ORCON` controls.
fn dissemination(bytes: &[u8], hits: &mut Vec<Hit>) {
    for word in [b"NOFORN".as_slice(), b"ORCON".as_slice()] {
        let mut i = 0;
        while let Some(p) = bytes[i..]
            .windows(word.len())
            .position(|c| c == word)
            .map(|p| p + i)
        {
            i = p + word.len();
            if !word_before(bytes, p) && !word_at(bytes, i) {
                hits.push(Hit {
                    kind: Kind::Dissemination,
                    start: p,
                    end: i,
                });
            }
        }
    }
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
                .take_while(|b| {
                    b.is_ascii_whitespace() || matches!(**b, b':' | b'#' | b'=' | b'"' | b'\'')
                })
                .count();
            start += skipped;
            if let Some(end) = value(bytes, start) {
                hits.push(Hit { kind, start, end });
            }
        }
    }
}

fn number(bytes: &[u8], at: usize, width: usize) -> Option<u32> {
    let run = digit_run(bytes, at);
    if run < width {
        return None;
    }
    Some(
        bytes[at..at + width]
            .iter()
            .fold(0u32, |n, b| n * 10 + u32::from(b - b'0')),
    )
}

fn ein_value(bytes: &[u8], at: usize) -> Option<usize> {
    if digit_run(bytes, at) != 2 || bytes.get(at + 2) != Some(&b'-') {
        return None;
    }
    if !(at + 10 <= bytes.len()) || digit_run(bytes, at + 3) != 7 || word_at(bytes, at + 10) {
        return None;
    }
    Some(at + 10)
}

fn itin_value(bytes: &[u8], at: usize) -> Option<usize> {
    if digit_run(bytes, at) != 3 || bytes[at] != b'9' || bytes.get(at + 3) != Some(&b'-') {
        return None;
    }
    let group = number(bytes, at + 4, 2)?;
    if bytes.get(at + 6) != Some(&b'-') || digit_run(bytes, at + 7) != 4 || word_at(bytes, at + 11)
    {
        return None;
    }
    if at + 11 > bytes.len() {
        return None;
    }
    let valid_group =
        (70..=88).contains(&group) || (90..=92).contains(&group) || (94..=99).contains(&group);
    valid_group.then_some(at + 11)
}

fn cage_char(c: u8) -> bool {
    c.is_ascii_digit() || matches!(c, b'A'..=b'H' | b'J'..=b'N' | b'P' | b'R'..=b'Z')
}

fn cage_value(bytes: &[u8], at: usize) -> Option<usize> {
    let tail = bytes.get(at..at + 5)?;
    (tail.iter().all(|c| cage_char(*c)) && !word_at(bytes, at + 5)).then_some(at + 5)
}

fn uei_value(bytes: &[u8], at: usize) -> Option<usize> {
    let tail = bytes.get(at..at + 12)?;
    (tail.iter().all(|c| cage_char(*c)) && !word_at(bytes, at + 12)).then_some(at + 12)
}

fn edipi_value(bytes: &[u8], at: usize) -> Option<usize> {
    if digit_run(bytes, at) != 10 {
        return None;
    }
    if bytes[at] == b'0' || word_at(bytes, at + 10) {
        return None;
    }
    Some(at + 10)
}

fn eccn_value(bytes: &[u8], at: usize) -> Option<usize> {
    let tail = bytes.get(at..at + 5)?;
    if !tail[0].is_ascii_digit()
        || !matches!(tail[1], b'A'..=b'E')
        || !tail[2..5].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let mut end = at + 5;
    if bytes.get(end) == Some(&b'.') {
        let suffix = bytes[end + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        if (1..=3).contains(&suffix) {
            end += 1 + suffix;
        }
    }
    (!word_at(bytes, end)).then_some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Kind> {
        let mut k = Kind::DEFAULT.to_vec();
        k.extend([
            Kind::Ein,
            Kind::Itin,
            Kind::Cage,
            Kind::Uei,
            Kind::Edipi,
            Kind::Eccn,
        ]);
        k
    }

    fn kinds(text: &str) -> Vec<Kind> {
        let mut found: Vec<Kind> = scan(text, &all()).into_iter().map(|h| h.kind).collect();
        found.sort();
        found
    }

    #[test]
    fn cui_banners_and_markers_are_found() {
        assert_eq!(kinds("CUI"), [Kind::Cui]);
        assert_eq!(kinds("CUI//SP-CTI"), [Kind::Cui]);
        assert_eq!(kinds("CUI//SP-EXPT"), [Kind::Cui]);
        assert!(kinds("cui").is_empty(), "lowercase is not a banner");
        assert!(kinds("xCUI").is_empty(), "inside a word");
        assert!(kinds("CUISINE").is_empty(), "a longer word");
        assert_eq!(
            kinds("CUI//SP-"),
            [Kind::Cui],
            "the bare banner is still a CUI marking"
        );
    }

    #[test]
    fn classification_banners_need_a_caveat_chain() {
        assert_eq!(
            kinds("TOP SECRET//SI//NOFORN"),
            [Kind::Classification, Kind::Dissemination]
        );
        assert_eq!(kinds("SECRET//REL TO USA, GBR"), [Kind::Classification]);
        assert_eq!(
            kinds("CONFIDENTIAL//NOFORN"),
            [Kind::Classification, Kind::Dissemination]
        );
        assert_eq!(
            kinds("TOP SECRET//SI//NOFORN//20330101"),
            [Kind::Classification, Kind::Dissemination]
        );
        assert!(kinds("SECRET").is_empty(), "bare level");
        assert!(kinds("this is a secret plan").is_empty(), "ordinary prose");
        assert!(kinds("SECRET//").is_empty(), "no caveat");
        assert!(kinds("SECRET//foo").is_empty(), "lowercase caveat");
        assert!(kinds("XSECRET//SI").is_empty(), "inside a word");
        assert!(kinds("SECRET //SI").is_empty(), "space before the chain");
    }

    #[test]
    fn dissemination_controls_are_standalone_uppercase_tokens() {
        assert_eq!(kinds("NOFORN"), [Kind::Dissemination]);
        assert_eq!(kinds("ORCON"), [Kind::Dissemination]);
        assert!(kinds("noforn").is_empty());
        assert!(kinds("NOFORNX").is_empty());
        assert!(kinds("xNOFORN").is_empty());
    }

    #[test]
    fn ein_needs_its_label_and_shape() {
        assert_eq!(kinds("EIN 12-3456789"), [Kind::Ein]);
        assert_eq!(kinds("\"ein\": \"12-3456789\""), [Kind::Ein]);
        assert!(kinds("12-3456789").is_empty(), "bare value");
        assert!(kinds("EIN 123-456789").is_empty(), "bad shape");
        assert!(kinds("xEIN 12-3456789").is_empty(), "label inside a word");
        assert!(kinds("EIN 12-34567890").is_empty(), "trailing digit");
    }

    #[test]
    fn itin_needs_its_label_and_a_historical_group_range() {
        assert_eq!(kinds("ITIN 912-70-5678"), [Kind::Itin]);
        assert_eq!(kinds("itin 900-88-1234"), [Kind::Itin]);
        assert!(kinds("ITIN 912-34-5678").is_empty(), "group out of range");
        assert!(kinds("912-70-5678").is_empty(), "bare value");
        assert!(kinds("ITIN 812-70-5678").is_empty(), "does not start at 9");
        assert!(kinds("xITIN 912-70-5678").is_empty(), "label inside a word");
    }

    #[test]
    fn cage_and_uei_need_their_labels_and_allowed_characters() {
        assert_eq!(kinds("CAGE 1A2B3"), [Kind::Cage]);
        assert_eq!(kinds("CAGE Code: 0A1B2"), [Kind::Cage]);
        assert!(kinds("CAGE 1I2B3").is_empty(), "I is not allowed");
        assert!(kinds("CAGE 1O2B3").is_empty(), "O is not allowed");
        assert!(kinds("1A2B3").is_empty(), "bare code");

        assert_eq!(kinds("UEI ABCD1234EFGH"), [Kind::Uei]);
        assert!(kinds("UEI ABCD1234EFG").is_empty(), "wrong length");
        assert!(kinds("UEI ABCD1234EFGI").is_empty(), "I is not allowed");
        assert!(kinds("ABCD1234EFGH").is_empty(), "bare code");
    }

    #[test]
    fn edipi_needs_its_label_and_ten_digits() {
        assert_eq!(kinds("EDIPI 1234567890"), [Kind::Edipi]);
        assert_eq!(kinds("DoD ID 1234567890"), [Kind::Edipi]);
        assert!(kinds("1234567890").is_empty(), "bare value");
        assert!(kinds("EDIPI 0234567890").is_empty(), "leading zero");
        assert!(kinds("EDIPI 123456789").is_empty(), "nine digits");
    }

    #[test]
    fn eccn_needs_its_label_and_the_export_control_shape() {
        assert_eq!(kinds("ECCN 5A002"), [Kind::Eccn]);
        assert_eq!(kinds("ECCN 9A610"), [Kind::Eccn]);
        assert_eq!(kinds("ECCN 5A002.a"), [Kind::Eccn]);
        assert!(kinds("5A002").is_empty(), "bare code");
        assert!(kinds("ECCN 5Z002").is_empty(), "not a product group");
        assert!(kinds("ECCN 5A02").is_empty(), "too short");
        assert!(kinds("xECCN 5A002").is_empty(), "label inside a word");
    }

    #[test]
    fn prose_and_code_without_markings_are_clean() {
        let text = "fn main() { let secret = 42; let ear = 1; let itar = 2; } // EAR ITAR FCI CTI CMMC FOUO";
        assert!(scan(text, &all()).is_empty());
        assert!(scan("FOUO SECRET CONFIDENTIAL", &all()).is_empty());
        assert!(scan("passport A12345678 driver license D1234567", &all()).is_empty());
    }

    #[test]
    fn keyword_kinds_are_off_by_default() {
        for kind in [
            Kind::Ein,
            Kind::Itin,
            Kind::Cage,
            Kind::Uei,
            Kind::Edipi,
            Kind::Eccn,
        ] {
            assert!(!Kind::DEFAULT.contains(&kind));
        }
        assert!(scan("EIN 12-3456789", &Kind::DEFAULT).is_empty());
        assert_eq!(scan("CUI", &Kind::DEFAULT)[0].kind, Kind::Cui);
    }

    #[test]
    fn hits_report_positions_and_never_text() {
        let text = "x CUI y";
        let hit = scan(text, &all())[0];
        assert_eq!((hit.start, hit.end), (2, 5));
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for text in [
            "CUI//",
            "CUI//SP-",
            "CUI//SP-ABC-",
            "SECRET//",
            "TOP SECRET//",
            "SECRET//REL TO ",
            "SECRET//REL TO USA,",
            "SECRET//FIVE EYES//",
            "EIN",
            "ITIN",
            "CAGE",
            "UEI",
            "EDIPI",
            "ECCN",
            "NOFORN",
            "        ",
            "\u{0}\u{1}\u{fffd}",
        ] {
            let _ = scan(text, &all());
        }
        let lossy = String::from_utf8_lossy(&[0xff, 0xfe, b'C', b'U', b'I', 0x80, b'/', b'/']);
        let _ = scan(&lossy, &all());
    }
}
