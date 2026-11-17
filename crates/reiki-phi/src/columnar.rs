//! Finds identifiers in tables: delimited text with a header row, SQL `INSERT ... (cols) VALUES`
//! and `COPY ... FROM stdin` blocks.
//!
//! A header names a column, and every cell under it is then checked as that kind of identifier,
//! so a bare nine-digit value under `ssn` is found even though nothing next to it says so.

use crate::detect::{self, Hit, Kind};
use reiki_detect::columnar::{self, Line};

/// Longest column list read after `INSERT INTO t (`.
const MAX_COLUMN_LIST: usize = 4096;
const DELIMITERS: [u8; 4] = *b"\t|,;";

/// The kind of identifier a column holds, from its header name.
fn column_kind(name: &str) -> Option<Kind> {
    let n: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let kind = match n.as_str() {
        "ssn" | "socialsecuritynumber" | "socialsecurity" | "ssnumber" => Kind::Ssn,
        "dob" | "dateofbirth" | "birthdate" | "birthday" => Kind::Dob,
        "mrn"
        | "medicalrecordnumber"
        | "memberid"
        | "subscriberid"
        | "policynumber"
        | "patientid"
        | "insuranceid"
        | "healthplanid"
        | "beneficiaryid"
        | "medicareid"
        | "mbi" => Kind::MedicalId,
        "accountnumber" | "accountno" | "acctnumber" | "acct" | "bankaccount"
        | "bankaccountnumber" => Kind::Account,
        "routingnumber" | "routing" | "aba" | "abanumber" | "abaroutingnumber" => Kind::Routing,
        "cardnumber" | "creditcard" | "creditcardnumber" | "ccnumber" | "pan" | "cardno" => {
            Kind::Card
        }
        "npi" | "npinumber" => Kind::Npi,
        "icd10" | "icd" | "icd10code" | "diagnosiscode" | "dxcode" => Kind::Icd10,
        "email" | "emailaddress" | "secondaryemail" | "pocemail" => Kind::Email,
        "phone"
        | "phonenumber"
        | "mobile"
        | "mobilephone"
        | "cell"
        | "telephone"
        | "secondaryphone"
        | "patientcontactphone"
        | "emergencycontactphone" => Kind::Phone,
        "address" | "address1" | "address2" | "addressline1" | "addressline2" | "streetaddress"
        | "homeaddress" | "mailingaddress" | "zip" | "zipcode" | "postalcode" | "diagnosis"
        | "diagnosisinfo" | "treatmentplan" | "cancerstage" | "medications" | "rawtranscript"
        | "patientname" => Kind::Column,
        _ if ["firstname", "lastname", "middlename", "fullname"]
            .iter()
            .any(|s| n.ends_with(s)) =>
        {
            Kind::Column
        }
        _ => return None,
    };
    Some(kind)
}

fn digits(value: &str) -> Vec<u8> {
    value.bytes().filter(u8::is_ascii_digit).collect()
}

/// True when `value`, the whole content of one cell, is an identifier of `kind`.
fn cell_ok(kind: Kind, value: &str) -> bool {
    let v = value.trim();
    let b = v.as_bytes();
    let plain = !b.is_empty() && b.iter().all(u8::is_ascii_digit);
    match kind {
        Kind::Ssn => {
            let d = digits(v);
            let shaped = (plain && b.len() == 9)
                || (b.len() == 11 && b[3] == b'-' && b[6] == b'-' && d.len() == 9);
            let n = |s: &[u8]| s.iter().fold(0u32, |a, c| a * 10 + u32::from(c - b'0'));
            shaped && detect::ssn_valid(n(&d[0..3]), n(&d[3..5]), n(&d[5..9]))
        }
        Kind::Card => {
            let d = digits(v);
            (13..=19).contains(&d.len())
                && b.iter()
                    .all(|c| c.is_ascii_digit() || matches!(c, b' ' | b'-'))
                && (b'2'..=b'6').contains(&d[0])
                && detect::luhn(&d)
        }
        Kind::Routing => {
            if !plain || b.len() != 9 {
                return false;
            }
            let d: Vec<u32> = b.iter().map(|c| u32::from(c - b'0')).collect();
            (3 * (d[0] + d[3] + d[6]) + 7 * (d[1] + d[4] + d[7]) + (d[2] + d[5] + d[8]))
                .is_multiple_of(10)
        }
        Kind::Account => plain && (6..=17).contains(&b.len()),
        Kind::Dob => detect::date_value(b, 0) == Some(b.len()),
        Kind::MedicalId => detect::id_value(b, 0) == Some(b.len()),
        Kind::Npi => {
            if !plain || b.len() != 10 {
                return false;
            }
            let mut full = b"80840".to_vec();
            full.extend_from_slice(b);
            detect::luhn(&full)
        }
        Kind::Icd10 => detect::icd_value(b, 0) == Some(b.len()),
        Kind::Email => {
            let mut found = Vec::new();
            detect::scan_email(b, &mut found);
            found.iter().any(|h| h.start == 0 && h.end == b.len())
        }
        Kind::Phone => {
            let d = digits(v);
            (d.len() == 10 || (d.len() == 11 && d[0] == b'1'))
                && b.iter().all(|c| {
                    c.is_ascii_digit() || matches!(c, b' ' | b'-' | b'.' | b'(' | b')' | b'+')
                })
        }
        Kind::Column => detect::plain_value(b),
        Kind::Field => false,
    }
}

/// Every hit in the tables of `text` for the wanted `kinds`.
pub fn scan_tables(text: &str, kinds: &[Kind]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let all = columnar::lines(text);
    let want = |k: Kind| kinds.contains(&k);
    let mut i = 0;
    while i < all.len() {
        let prev = i.checked_sub(1).map(|p| &all[p]);
        let Some((delim, map, width)) = header(&all[i], prev, &want) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < all.len() {
            if all[j].text.trim().is_empty() {
                break;
            }
            // A row of another width is skipped, not treated as the end: a quoted cell that spans
            // lines leaves a short line behind.
            let cells = columnar::split_cells(all[j].text, delim, all[j].start);
            if cells.len() == width {
                for (idx, (s, e)) in cells.iter().enumerate() {
                    if let Some(kind) = map[idx] {
                        if cell_ok(kind, &text[*s..*e]) {
                            hits.push(Hit {
                                kind,
                                start: *s,
                                end: *e,
                            });
                        }
                    }
                }
            }
            j += 1;
        }
        i = j;
    }
    sql(text, &want, &mut hits);
    columnar::copy_blocks(text, &all, &want, column_kind, cell_ok, &mut hits);
    hits
}

/// True when `delim` starts a table on this line: the line is the first one in the document or
/// follows a blank line or a line without that delimiter.
fn starts_table(prev: Option<&Line<'_>>, delim: u8) -> bool {
    match prev {
        None => true,
        Some(p) => p.text.trim().is_empty() || columnar::split_cells(p.text, delim, 0).len() < 2,
    }
}

/// The delimiter, the kind of each column and the column count when `line` is a table header.
///
/// A header starts its table and, when it maps to [`Kind::Column`], which accepts almost any
/// cell, at least half of its non-empty cells must be known column names. Without that, a data
/// row such as `alice,address` would be read as a header and the next row flagged as a column.
#[allow(clippy::type_complexity)]
fn header(
    line: &Line<'_>,
    prev: Option<&Line<'_>>,
    want: &dyn Fn(Kind) -> bool,
) -> Option<(u8, Vec<Option<Kind>>, usize)> {
    for delim in DELIMITERS {
        let cells = columnar::split_cells(line.text, delim, 0);
        if cells.len() < 2 || !starts_table(prev, delim) {
            continue;
        }
        let known = cells
            .iter()
            .filter(|(s, e)| column_kind(&line.text[*s..*e]).is_some())
            .count();
        let nonempty = cells
            .iter()
            .filter(|(s, e)| !line.text[*s..*e].is_empty())
            .count();
        let map: Vec<Option<Kind>> = cells
            .iter()
            .map(|(s, e)| column_kind(&line.text[*s..*e]).filter(|k| want(*k)))
            .collect();
        if !map.iter().any(Option::is_some) {
            continue;
        }
        if map.contains(&Some(Kind::Column)) && known * 2 < nonempty {
            continue;
        }
        return Some((delim, map, cells.len()));
    }
    None
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'"' | b'`' | b'.' | b'[' | b']')
}

/// `INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y');`
fn sql(text: &str, want: &dyn Fn(Kind) -> bool, hits: &mut Vec<Hit>) {
    let b = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find("insert into") {
        let mut i = from + p + "insert into".len();
        from = i;
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        while b.get(i).is_some_and(|c| is_ident(*c)) {
            i += 1;
        }
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        if b.get(i) != Some(&b'(') {
            continue;
        }
        // A column list is short and holds no '('. Stopping there keeps a body of unclosed INSERTs
        // from rescanning ahead on every occurrence.
        let Some(close) = b[i + 1..]
            .iter()
            .take(MAX_COLUMN_LIST)
            .take_while(|c| **c != b'(')
            .position(|c| *c == b')')
            .map(|q| i + 1 + q)
        else {
            continue;
        };
        let map: Vec<Option<Kind>> = text[i + 1..close]
            .split(',')
            .map(|name| {
                column_kind(name.trim().trim_matches(['"', '`', '[', ']'])).filter(|k| want(*k))
            })
            .collect();
        i = close + 1;
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        if !lower[i..].starts_with("values") {
            continue;
        }
        i += "values".len();
        if map.iter().all(Option::is_none) {
            continue;
        }
        i = columnar::tuples(text, i, &map, hits, cell_ok);
        from = from.max(i);
    }
}

#[cfg(test)]
mod tests {
    use crate::detect::{scan, Kind};

    fn all() -> Vec<Kind> {
        let mut k = Kind::DEFAULT.to_vec();
        k.extend([Kind::Field, Kind::Email, Kind::Phone]);
        k
    }

    fn found(text: &str) -> Vec<Kind> {
        let mut kinds: Vec<Kind> = scan(text, &all()).into_iter().map(|h| h.kind).collect();
        kinds.sort();
        kinds
    }

    #[test]
    fn a_csv_header_gives_bare_numbers_their_meaning() {
        let csv =
            "id,ssn,first_name,dob\n1,123456789,Jane,02/03/1980\n2,234-56-7890,Joe,1975-12-31\n";
        assert_eq!(
            found(csv),
            [
                Kind::Ssn,
                Kind::Ssn,
                Kind::Dob,
                Kind::Dob,
                Kind::Column,
                Kind::Column
            ]
        );
        assert_eq!(
            found("id,ssn\n1,123456789\n"),
            [Kind::Ssn],
            "nothing next to the digits says ssn but the header"
        );
    }

    #[test]
    fn other_delimiters_and_markdown_tables_work() {
        assert_eq!(found("ssn\tname\n123456789\tx\n"), [Kind::Ssn]);
        assert_eq!(
            found("mrn|dob\nA1234567|02/03/1980\n"),
            [Kind::Dob, Kind::MedicalId]
        );
        assert_eq!(found("ssn;city\n123456789;x\n"), [Kind::Ssn]);
        assert_eq!(
            found("| ssn | last_name |\n|---|---|\n| 123456789 | Doe |\n"),
            [Kind::Ssn, Kind::Column]
        );
    }

    #[test]
    fn quoted_cells_keep_their_delimiters() {
        let csv = "last_name,ssn\n\"Doe, Jane\",123456789\n";
        assert_eq!(found(csv), [Kind::Ssn, Kind::Column]);
    }

    #[test]
    fn sql_inserts_map_values_to_their_columns() {
        let sql = "INSERT INTO patients (id, first_name, ssn, dob) VALUES (1, 'Jane', '123456789', '1980-02-03'), (2, 'Joe', '234567890', NULL);";
        assert_eq!(
            found(sql),
            [Kind::Ssn, Kind::Ssn, Kind::Dob, Kind::Column, Kind::Column]
        );
        assert_eq!(
            found("insert into t (\"ssn\") values ('1''2'),('123456789');"),
            [Kind::Ssn]
        );
    }

    #[test]
    fn copy_blocks_are_read_as_tab_separated_rows() {
        let copy = "COPY patients (id, ssn, last_name) FROM stdin;\n1\t123456789\tDoe\n2\t234567890\tRoe\n\\.\n";
        assert_eq!(
            found(copy),
            [Kind::Ssn, Kind::Ssn, Kind::Column, Kind::Column]
        );
    }

    #[test]
    fn a_data_row_is_not_a_table_header() {
        let csv = "name,notes\nalice,address\nbob,phone\n";
        assert!(
            scan(csv, &Kind::DEFAULT).is_empty(),
            "the data row must not become a header"
        );
    }

    #[test]
    fn a_header_after_a_blank_line_still_names_its_columns() {
        let text = "Some notes about the export.\n\nid,ssn,notes\n1,123456789,ok\n";
        assert_eq!(found(text), [Kind::Ssn]);
    }

    #[test]
    fn a_header_may_have_unmapped_columns_when_most_cells_are_known() {
        let csv = "id,ssn,first_name,dob,notes\n1,123456789,Jane,02/03/1980,seen\n";
        assert_eq!(
            found(csv),
            [Kind::Ssn, Kind::Dob, Kind::Column],
            "three of five header cells are known, so the table is read"
        );
    }

    #[test]
    fn tables_without_identifying_headers_or_data_are_clean() {
        assert!(found("id,price,qty\n1,9.99,3\n").is_empty());
        assert!(found("ssn,notes\nN/A,ok\n,\n").is_empty());
        assert!(
            found("first_name,last_name\nstring,string\n").is_empty(),
            "a schema, not data"
        );
        assert!(found("INSERT INTO t (a, b) VALUES (1, 2);").is_empty());
        assert!(
            found("INSERT INTO t VALUES ('123456789');").is_empty(),
            "no column list to map"
        );
    }

    #[test]
    fn a_cell_is_reported_once_when_text_and_column_scans_both_see_it() {
        let hits = scan("ssn,x\n123-45-6789,y\n", &all());
        assert_eq!(hits.len(), 1);
    }

    fn table_values(text: &str) -> Vec<&str> {
        super::scan_tables(text, &all())
            .into_iter()
            .map(|h| &text[h.start..h.end])
            .collect()
    }

    #[test]
    fn a_ragged_row_does_not_end_the_table() {
        assert_eq!(
            table_values("ssn,last_name\n123456789,Doe\nx,y,z\n234567890,Roe\n"),
            ["123456789", "Doe", "234567890", "Roe"]
        );
        assert_eq!(
            table_values("ssn,last_name\n123456789,\"Doe\nJane\"\n234567890,Roe\n"),
            ["123456789", "Doe", "234567890", "Roe"],
            "a quoted cell spanning lines leaves a short line behind"
        );
    }

    #[test]
    fn a_normal_table_is_read_in_full() {
        assert_eq!(
            table_values("ssn,last_name\n123456789,Doe\n234567890,Roe\n"),
            ["123456789", "Doe", "234567890", "Roe"]
        );
    }

    #[test]
    fn a_blank_line_still_ends_the_table() {
        assert_eq!(
            table_values("ssn,last_name\n123456789,Doe\n\n234567890,Roe\n"),
            ["123456789", "Doe"]
        );
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for text in [
            "ssn,\n\"",
            "ssn,x\n\"unterminated,1",
            "insert into t (ssn) values (",
            "insert into t (ssn",
            "copy t (ssn) from stdin;\n",
            "|\n|",
            ",,,,\n,,,,",
            "insert into t (ssn) values ('",
        ] {
            let _ = scan(text, &all());
        }
    }
}
