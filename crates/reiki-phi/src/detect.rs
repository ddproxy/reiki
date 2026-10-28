//! Finds identifiers that mark protected health information (PHI) and nonpublic personal
//! information (NPPI) in text.
//!
//! These are heuristics: structured identifiers are checked for shape and checksum, and the
//! ones that look like any other number are only reported next to a keyword. Free-text names
//! and addresses are not detected. A hit never carries the matched text, only its position.

use reiki_detect::{digit_run, keyword_before, word_at, word_before};

pub(crate) use reiki_detect::luhn;

/// What a hit is an identifier of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Ssn,
    Card,
    Routing,
    Account,
    Dob,
    MedicalId,
    Npi,
    Icd10,
    Column,
    Field,
    Email,
    Phone,
}

impl Kind {
    /// Kinds that are on unless the config narrows the list.
    pub const DEFAULT: [Kind; 9] = [
        Kind::Ssn,
        Kind::Card,
        Kind::Routing,
        Kind::Account,
        Kind::Dob,
        Kind::MedicalId,
        Kind::Npi,
        Kind::Icd10,
        Kind::Column,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Ssn => "ssn",
            Kind::Card => "card",
            Kind::Routing => "routing",
            Kind::Account => "account",
            Kind::Dob => "dob",
            Kind::MedicalId => "medical_id",
            Kind::Npi => "npi",
            Kind::Icd10 => "icd10",
            Kind::Column => "column",
            Kind::Field => "field",
            Kind::Email => "email",
            Kind::Phone => "phone",
        }
    }

    /// Every kind, including the ones that are off by default.
    pub const ALL: [Kind; 12] = [
        Kind::Ssn,
        Kind::Card,
        Kind::Routing,
        Kind::Account,
        Kind::Dob,
        Kind::MedicalId,
        Kind::Npi,
        Kind::Icd10,
        Kind::Column,
        Kind::Field,
        Kind::Email,
        Kind::Phone,
    ];

    pub fn parse(label: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.label() == label)
    }
}

/// A byte range of the scanned text that looks like an identifier.
pub type Hit = reiki_detect::Hit<Kind>;

const SSN_WORDS: &[&str] = &[
    "ssn",
    "social security",
    "social sec",
    "social_security_number",
];
const ROUTING_WORDS: &[&str] = &["routing", "aba"];
const ACCOUNT_WORDS: &[&str] = &[
    "account number",
    "account_number",
    "account no",
    "account #",
    "acct",
];
const DOB_WORDS: &[&str] = &[
    "dob",
    "d.o.b",
    "date of birth",
    "date_of_birth",
    "dateofbirth",
    "birth date",
    "birth_date",
    "birthdate",
    "born on",
];
const NPI_WORDS: &[&str] = &["npi"];
const MEDICAL_WORDS: &[&str] = &[
    "mrn",
    "medical record number",
    "medical_record_number",
    "medical record no",
    "member id",
    "member_id",
    "subscriber id",
    "subscriber_id",
    "policy number",
    "policy_number",
    "patient id",
    "patient_id",
    "health plan id",
    "beneficiary id",
    "medicare id",
    "medicare number",
    "medicaid id",
    "mbi",
    "insurance id",
];
const ICD_WORDS: &[&str] = &[
    "icd-10",
    "icd10",
    "icd_10",
    "icd",
    "diagnosis code",
    "diagnosis_code",
    "dx code",
];

/// Field names that carry identifying data in JSON, YAML and log lines.
const FIELD_NAMES: &[&str] = &[
    "first_name",
    "firstname",
    "last_name",
    "lastname",
    "middle_name",
    "full_name",
    "fullname",
    "patient_name",
    "patientname",
    "address_line_1",
    "address_line_2",
    "addressline1",
    "addressline2",
    "street_address",
    "home_address",
    "mailing_address",
    "zip_code",
    "zipcode",
    "postal_code",
    "phone_number",
    "phonenumber",
    "mobile_phone",
    "patient_contact_phone",
    "emergency_contact_phone",
    "diagnosis",
    "diagnosis_info",
    "treatment_plan",
    "treatmentplan",
    "cancer_stage",
    "medications",
    "raw_transcript",
];

/// Values that name a type or stand for nothing, so a field holding one is a schema, not data.
const NON_DATA: &[&str] = &[
    "str",
    "string",
    "int",
    "integer",
    "text",
    "varchar",
    "char",
    "date",
    "datetime",
    "timestamp",
    "boolean",
    "bool",
    "null",
    "none",
    "nil",
    "true",
    "false",
    "number",
    "object",
    "array",
    "uuid",
    "n/a",
    "na",
    "unknown",
    "redacted",
    "required",
    "optional",
    "any",
    "list",
    "dict",
    "float",
    "bytes",
];

/// Every hit for the wanted `kinds`, in order of position within each kind.
pub fn scan(text: &str, kinds: &[Kind]) -> Vec<Hit> {
    let bytes = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let lower = lower.as_bytes();
    let mut hits = Vec::new();
    let want = |k: Kind| kinds.contains(&k);
    if want(Kind::Ssn) {
        ssn(bytes, lower, &mut hits);
    }
    if want(Kind::Card) {
        card(bytes, &mut hits);
    }
    if want(Kind::Routing) {
        routing(bytes, lower, &mut hits);
    }
    if want(Kind::Npi) {
        npi(bytes, lower, &mut hits);
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
    if want(Kind::Dob) {
        keyed(bytes, lower, DOB_WORDS, Kind::Dob, date_value, &mut hits);
    }
    if want(Kind::MedicalId) {
        keyed(
            bytes,
            lower,
            MEDICAL_WORDS,
            Kind::MedicalId,
            id_value,
            &mut hits,
        );
    }
    if want(Kind::Icd10) {
        keyed(bytes, lower, ICD_WORDS, Kind::Icd10, icd_value, &mut hits);
    }
    if want(Kind::Field) {
        field(bytes, lower, &mut hits);
    }
    if want(Kind::Email) {
        email(bytes, &mut hits);
    }
    if want(Kind::Phone) {
        phone(bytes, &mut hits);
    }
    reiki_detect::merge_hits(&mut hits, crate::columnar::scan_tables(text, kinds));
    hits
}

fn plain_runs(bytes: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() && !word_before(bytes, i) {
                let n = digit_run(bytes, i);
                let start = i;
                i += n;
                if !word_at(bytes, start + n) {
                    return Some((start, start + n));
                }
            } else {
                i += 1;
            }
        }
        None
    })
}

pub(crate) fn ssn_valid(area: u32, group: u32, serial: u32) -> bool {
    area != 0 && area != 666 && area < 900 && group != 0 && serial != 0
}

fn ssn(bytes: &[u8], lower: &[u8], hits: &mut Vec<Hit>) {
    let number = |s: &[u8]| s.iter().fold(0u32, |n, b| n * 10 + u32::from(b - b'0'));
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() || word_before(bytes, i) {
            i += 1;
            continue;
        }
        let n = digit_run(bytes, i);
        // 123-45-6789 or, with a keyword, 123 45 6789
        if n == 3 && i + 11 <= bytes.len() {
            let sep = bytes[i + 3];
            let s = &bytes[i..i + 11];
            let shaped = (sep == b'-' || sep == b' ')
                && s[4..6].iter().all(u8::is_ascii_digit)
                && s[6] == sep
                && s[7..11].iter().all(u8::is_ascii_digit)
                && !word_at(bytes, i + 11)
                && !(bytes.get(i + 11) == Some(&b'-')
                    && bytes.get(i + 12).is_some_and(u8::is_ascii_digit));
            if shaped
                && ssn_valid(number(&s[0..3]), number(&s[4..6]), number(&s[7..11]))
                && (sep == b'-' || keyword_before(lower, i, SSN_WORDS, 32))
            {
                hits.push(Hit {
                    kind: Kind::Ssn,
                    start: i,
                    end: i + 11,
                });
                i += 11;
                continue;
            }
        }
        // nine digits in a row need a keyword
        if n == 9
            && !word_at(bytes, i + 9)
            && keyword_before(lower, i, SSN_WORDS, 32)
            && ssn_valid(
                number(&bytes[i..i + 3]),
                number(&bytes[i + 3..i + 5]),
                number(&bytes[i + 5..i + 9]),
            )
        {
            hits.push(Hit {
                kind: Kind::Ssn,
                start: i,
                end: i + 9,
            });
        }
        i += n;
    }
}

/// Payment card numbers: 13 to 19 digits in groups split by single spaces or dashes, a plausible
/// leading digit, and a valid Luhn checksum.
fn card(bytes: &[u8], hits: &mut Vec<Hit>) {
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
            hits.push(Hit {
                kind: Kind::Card,
                start: i,
                end: j,
            });
            i = j;
        } else {
            i += digit_run(bytes, i);
        }
    }
}

fn routing(bytes: &[u8], lower: &[u8], hits: &mut Vec<Hit>) {
    for (s, e) in plain_runs(bytes) {
        if e - s != 9 || !keyword_before(lower, s, ROUTING_WORDS, 30) {
            continue;
        }
        let d: Vec<u32> = bytes[s..e].iter().map(|b| u32::from(b - b'0')).collect();
        let sum = 3 * (d[0] + d[3] + d[6]) + 7 * (d[1] + d[4] + d[7]) + (d[2] + d[5] + d[8]);
        if sum.is_multiple_of(10) {
            hits.push(Hit {
                kind: Kind::Routing,
                start: s,
                end: e,
            });
        }
    }
}

fn npi(bytes: &[u8], lower: &[u8], hits: &mut Vec<Hit>) {
    for (s, e) in plain_runs(bytes) {
        if e - s != 10 || !keyword_before(lower, s, NPI_WORDS, 12) {
            continue;
        }
        let mut full = b"80840".to_vec();
        full.extend_from_slice(&bytes[s..e]);
        if luhn(&full) {
            hits.push(Hit {
                kind: Kind::Npi,
                start: s,
                end: e,
            });
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
            if word_before(lower, at) {
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

fn account_value(bytes: &[u8], at: usize) -> Option<usize> {
    let n = digit_run(bytes, at);
    ((6..=17).contains(&n) && !word_at(bytes, at + n)).then_some(at + n)
}

pub(crate) fn id_value(bytes: &[u8], at: usize) -> Option<usize> {
    let token = bytes[at..]
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'-')
        .count();
    let digits = bytes[at..at + token]
        .iter()
        .filter(|b| b.is_ascii_digit())
        .count();
    ((5..=20).contains(&token) && digits >= 4).then_some(at + token)
}

pub(crate) fn date_value(bytes: &[u8], at: usize) -> Option<usize> {
    let num = |from: usize, max: usize| {
        let n = digit_run(bytes, from).min(max + 1);
        (n >= 1 && n <= max).then_some((
            from + n,
            bytes[from..from + n]
                .iter()
                .fold(0u32, |a, b| a * 10 + u32::from(b - b'0')),
        ))
    };
    // 2001-02-03
    if digit_run(bytes, at) == 4 && bytes.get(at + 4) == Some(&b'-') {
        let (m_end, m) = num(at + 5, 2)?;
        if bytes.get(m_end) == Some(&b'-') {
            let (d_end, d) = num(m_end + 1, 2)?;
            return ((1..=12).contains(&m) && (1..=31).contains(&d) && !word_at(bytes, d_end))
                .then_some(d_end);
        }
        return None;
    }
    // 2/3/2001 or 02-03-01
    let (m_end, m) = num(at, 2)?;
    let sep = *bytes.get(m_end)?;
    if sep != b'/' && sep != b'-' {
        return None;
    }
    let (d_end, d) = num(m_end + 1, 2)?;
    if bytes.get(d_end) != Some(&sep) {
        return None;
    }
    let y = digit_run(bytes, d_end + 1);
    let end = d_end + 1 + y;
    ((1..=12).contains(&m) && (1..=31).contains(&d) && (y == 2 || y == 4) && !word_at(bytes, end))
        .then_some(end)
}

/// An ICD-10 code such as C50.911 or E11.9.
pub(crate) fn icd_value(bytes: &[u8], at: usize) -> Option<usize> {
    let b = |i: usize| bytes.get(at + i).copied();
    let first = b(0)?;
    if !first.is_ascii_uppercase() || first == b'U' || !b(1)?.is_ascii_digit() {
        return None;
    }
    if !(b(2)?.is_ascii_digit() || matches!(b(2)?, b'A' | b'B')) {
        return None;
    }
    let mut end = at + 3;
    if bytes.get(end) == Some(&b'.') {
        let tail = bytes[end + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        if !(1..=4).contains(&tail) {
            return None;
        }
        end += 1 + tail;
    }
    (!word_at(bytes, end)).then_some(end)
}

/// A sensitive field name followed by a value that looks like data: `"first_name": "Jane"`.
fn field(bytes: &[u8], lower: &[u8], hits: &mut Vec<Hit>) {
    for name in FIELD_NAMES {
        let name = name.as_bytes();
        let mut from = 0;
        while let Some(at) = lower[from..]
            .windows(name.len())
            .position(|c| c == name)
            .map(|p| p + from)
        {
            from = at + name.len();
            // a prefix such as primary_ is fine; a longer word such as first_name_ct is not
            if (at > 0 && lower[at - 1].is_ascii_alphanumeric()) || word_at(lower, from) {
                continue;
            }
            let mut p = from;
            if matches!(bytes.get(p), Some(b'"' | b'\'')) {
                p += 1;
            }
            while bytes.get(p) == Some(&b' ') {
                p += 1;
            }
            if !matches!(bytes.get(p), Some(b':' | b'=')) {
                continue;
            }
            p += 1;
            while bytes.get(p) == Some(&b' ') {
                p += 1;
            }
            if let Some((start, end)) = data_value(bytes, p) {
                hits.push(Hit {
                    kind: Kind::Field,
                    start,
                    end,
                });
            }
        }
    }
}

/// The value at `at` when it reads as real data and not as a type, a call or another identifier.
fn data_value(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let quote = bytes.get(at).copied().filter(|q| matches!(q, b'"' | b'\''));
    let (start, len) = match quote {
        Some(q) => {
            let inner = at + 1;
            let len = bytes[inner..]
                .iter()
                .take(80)
                .position(|&c| c == q || c == b'\n')?;
            if bytes[inner + len] != q {
                return None;
            }
            (inner, len)
        }
        None => (
            at,
            bytes[at..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == b'-')
                .count(),
        ),
    };
    let value = &bytes[start..start + len];
    let allowed = |c: &u8| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b' ' | b'.' | b',' | b'\'' | b'-' | b'#' | b'/' | b'@' | b'+'
            )
    };
    if len < 2 || !value.iter().all(allowed) || !value.iter().any(u8::is_ascii_alphanumeric) {
        return None;
    }
    let word = String::from_utf8_lossy(value).to_ascii_lowercase();
    if NON_DATA.contains(&word.trim()) {
        return None;
    }
    if quote.is_none()
        && matches!(
            bytes.get(start + len),
            Some(b'(' | b'[' | b'.' | b'_' | b'<' | b':')
        )
    {
        return None;
    }
    Some((start, start + len))
}

pub(crate) fn scan_email(bytes: &[u8], hits: &mut Vec<Hit>) {
    email(bytes, hits);
}

/// A table cell that reads as data: not empty, a type name or a placeholder.
pub(crate) fn plain_value(bytes: &[u8]) -> bool {
    let allowed = |c: &u8| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b' ' | b'.' | b',' | b'\'' | b'-' | b'#' | b'/' | b'@' | b'+' | b'(' | b')'
            )
    };
    let word = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    bytes.len() >= 2
        && bytes.iter().all(allowed)
        && bytes.iter().any(u8::is_ascii_alphanumeric)
        && !NON_DATA.contains(&word.trim())
}

fn email(bytes: &[u8], hits: &mut Vec<Hit>) {
    let local = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-');
    for at in (0..bytes.len()).filter(|&i| bytes[i] == b'@') {
        let start = bytes[..at]
            .iter()
            .rposition(|&b| !local(b))
            .map_or(0, |p| p + 1);
        let tail = bytes[at + 1..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'.' || **b == b'-')
            .count();
        let domain = &bytes[at + 1..at + 1 + tail];
        let dot = domain.iter().rposition(|&b| b == b'.');
        if start < at
            && tail > 3
            && dot.is_some_and(|d| {
                d > 0 && domain.len() - d > 2 && domain[d + 1..].iter().all(u8::is_ascii_alphabetic)
            })
        {
            hits.push(Hit {
                kind: Kind::Email,
                start,
                end: at + 1 + tail,
            });
        }
    }
}

/// US numbers written with parentheses or separators, such as (555) 234-5678 or 555-234-5678.
fn phone(bytes: &[u8], hits: &mut Vec<Hit>) {
    let sep = |b: Option<&u8>| matches!(b, Some(b'-' | b'.' | b' '));
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let paren = bytes[i] == b'(';
        let mut j = i + usize::from(paren);
        let ok_start = (bytes[i].is_ascii_digit() || paren) && !word_before(bytes, i);
        if ok_start
            && bytes.get(j).is_some_and(|b| (b'2'..=b'9').contains(b))
            && digit_run(bytes, j) >= 3
        {
            let area_ok = digit_run(bytes, j) == 3;
            j += 3;
            if paren && bytes.get(j) == Some(&b')') {
                j += 1;
            }
            let had_sep = paren || sep(bytes.get(j));
            if sep(bytes.get(j)) {
                j += 1;
            }
            if area_ok && had_sep && digit_run(bytes, j) == 3 {
                j += 3;
                if sep(bytes.get(j)) {
                    j += 1;
                }
                if digit_run(bytes, j) == 4 && !word_at(bytes, j + 4) {
                    hits.push(Hit {
                        kind: Kind::Phone,
                        start,
                        end: j + 4,
                    });
                    i = j + 4;
                    continue;
                }
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str, wanted: &[Kind]) -> Vec<Kind> {
        scan(text, wanted).into_iter().map(|h| h.kind).collect()
    }

    fn all() -> Vec<Kind> {
        let mut k = Kind::DEFAULT.to_vec();
        k.extend([Kind::Email, Kind::Phone]);
        k
    }

    #[test]
    fn an_ssn_is_found_with_dashes_and_excludes_invalid_areas() {
        assert_eq!(kinds("ssn is 123-45-6789 ok", &all()), [Kind::Ssn]);
        for bad in [
            "000-12-3456",
            "666-12-3456",
            "900-12-3456",
            "123-00-4567",
            "123-45-0000",
        ] {
            assert!(kinds(bad, &all()).is_empty(), "{bad}");
        }
        assert!(kinds("order 123-45-6789-01", &all()).is_empty());
    }

    #[test]
    fn nine_digits_and_spaced_ssns_need_a_keyword() {
        assert!(kinds("id 123456789 here", &all()).is_empty());
        assert_eq!(kinds("SSN: 123456789", &all()), [Kind::Ssn]);
        assert!(kinds("part 123 45 6789", &all()).is_empty());
        assert_eq!(kinds("social security 123 45 6789", &all()), [Kind::Ssn]);
    }

    #[test]
    fn cards_need_a_valid_checksum_and_leading_digit() {
        assert_eq!(kinds("pay 4111 1111 1111 1111 now", &all()), [Kind::Card]);
        assert_eq!(kinds("4111-1111-1111-1111", &all()), [Kind::Card]);
        assert!(
            kinds("4111 1111 1111 1112", &all()).is_empty(),
            "bad checksum"
        );
        assert!(
            kinds("ts 1793212908123", &all()).is_empty(),
            "timestamps start with 1"
        );
        assert!(
            kinds("abc4111111111111111", &all()).is_empty(),
            "inside a word"
        );
    }

    #[test]
    fn routing_numbers_need_a_keyword_and_a_checksum() {
        assert_eq!(kinds("ABA routing: 021000021", &all()), [Kind::Routing]);
        assert!(kinds("number 021000021", &all()).is_empty());
        assert!(kinds("routing 021000022", &all()).is_empty());
    }

    #[test]
    fn keyed_identifiers_follow_their_keyword() {
        assert_eq!(
            kinds("account number: 000123456789", &all()),
            [Kind::Account]
        );
        assert!(kinds("account number: 123", &all()).is_empty());
        assert_eq!(kinds("DOB: 02/03/1980", &all()), [Kind::Dob]);
        assert_eq!(kinds("date of birth 1980-02-03", &all()), [Kind::Dob]);
        assert!(kinds("dob 13/45/1980", &all()).is_empty());
        assert_eq!(kinds("MRN: A1234567", &all()), [Kind::MedicalId]);
        assert!(kinds("mrn: abcdefg", &all()).is_empty(), "no digits");
        assert_eq!(kinds("provider NPI 1234567893", &all()), [Kind::Npi]);
        assert!(kinds("npi 1234567890", &all()).is_empty());
    }

    #[test]
    fn email_and_phone_are_found_when_asked_for() {
        assert_eq!(
            kinds("mail jane.doe+x@example.com now", &all()),
            [Kind::Email]
        );
        assert_eq!(kinds("call (555) 234-5678", &all()), [Kind::Phone]);
        assert_eq!(kinds("call 555-234-5678", &all()), [Kind::Phone]);
        assert!(kinds("5552345678", &all()).is_empty());
        assert!(
            kinds("jane@example.com 555-234-5678", &Kind::DEFAULT).is_empty(),
            "off by default"
        );
        assert!(kinds("user@localhost", &all()).is_empty());
    }

    #[test]
    fn hits_report_positions_and_never_text() {
        let text = "x 123-45-6789 y";
        let hit = scan(text, &all())[0];
        assert_eq!((hit.start, hit.end), (2, 13));
    }

    #[test]
    fn ordinary_prose_and_code_are_clean() {
        let text =
            "fn main() { let n = 42; println!(\"{n}\"); } // 2026-10-28 build 1.2.3 port 4097";
        assert!(scan(text, &all()).is_empty());
        assert!(scan("é — 日本語 and 123-45-6789", &all()).len() == 1);
    }

    #[test]
    fn underscore_keywords_catch_structured_fields() {
        assert_eq!(
            kinds("\"date_of_birth\": \"1980-02-03\"", &all()),
            [Kind::Dob]
        );
        assert_eq!(
            kinds("{\"member_id\": \"A1234567\"}", &all()),
            [Kind::MedicalId]
        );
        assert_eq!(kinds("medicare id 1EG4TE5MK73", &all()), [Kind::MedicalId]);
        assert_eq!(
            kinds("\"social_security_number\": \"123456789\"", &all()),
            [Kind::Ssn]
        );
    }

    #[test]
    fn icd10_codes_follow_their_keyword() {
        assert_eq!(kinds("ICD-10: C50.911", &all()), [Kind::Icd10]);
        assert_eq!(kinds("diagnosis_code = E11.9", &all()), [Kind::Icd10]);
        assert!(kinds("icd: hello", &all()).is_empty());
        assert!(kinds("code C50.911 alone", &all()).is_empty());
    }

    #[test]
    fn field_values_are_found_by_name_and_only_when_they_read_as_data() {
        let field = [Kind::Field];
        assert_eq!(
            kinds(
                "{\"first_name\": \"Jane\", \"last_name\": \"O'Neil\"}",
                &field
            ),
            [Kind::Field, Kind::Field]
        );
        assert_eq!(kinds("last_name: Doe", &field), [Kind::Field]);
        assert_eq!(
            kinds("{\"primary_first_name\": \"Jane\"}", &field),
            [Kind::Field]
        );
        assert_eq!(kinds("{\"zip_code\": \"02139\"}", &field), [Kind::Field]);
        for clean in [
            "first_name: str",
            "first_name = Column(String)",
            "{\"first_name\": \"string\"}",
            "first_name: Mapped[str]",
            "{\"first_name_ct\": \"abcdef\"}",
            "{\"first_name\": \"\"}",
            "{\"first_name\": null}",
            "first_name = request.first_name",
            "{\"first_name\": \"{{ user }}\"}",
            "{\"first_name\": \"[[REIKI_TERM_aaaa]]\"}",
        ] {
            assert!(kinds(clean, &field).is_empty(), "{clean}");
        }
    }

    #[test]
    fn field_is_off_by_default() {
        assert!(!Kind::DEFAULT.contains(&Kind::Field));
        assert!(Kind::DEFAULT.contains(&Kind::Icd10) && Kind::DEFAULT.contains(&Kind::Column));
    }
}
