//! Finds financial identifiers in tables: delimited text with a header row, SQL
//! `INSERT ... (cols) VALUES` and `COPY ... FROM stdin` blocks.
//!
//! A header names a column, and every cell under it is then checked as that kind of identifier,
//! so a bare account number under `account` is found even though nothing next to it says so.

use crate::detect::{self, Hit, Kind};
use reiki_detect::columnar::{self, Line};

const DELIMITERS: [u8; 4] = *b"\t|,;";

/// The kind of identifier a column holds, from its header name.
fn column_kind(name: &str) -> Option<Kind> {
    let n: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let kind = match n.as_str() {
        "iban" | "ibannumber" | "ibanumber" => Kind::Iban,
        "swift" | "swiftcode" | "swiftbic" | "bic" | "biccode" => Kind::Swift,
        "cusip" | "cusipnumber" => Kind::Cusip,
        "isin" | "isinnumber" => Kind::Isin,
        "wallet" | "walletaddress" | "crypto" | "cryptoaddress" | "btcaddress"
        | "bitcoinaddress" | "ethaddress" | "ethereumaddress" => Kind::Crypto,
        "account" | "accountnumber" | "accountno" | "acct" | "acctnumber" | "bankaccount"
        | "bankaccountnumber" => Kind::Account,
        "sedol" | "sedolcode" | "sedolnumber" => Kind::Sedol,
        _ => return None,
    };
    Some(kind)
}

/// True when `value`, the whole content of one cell, is an identifier of `kind`.
fn cell_ok(kind: Kind, value: &str) -> bool {
    let v = value.trim().as_bytes();
    match kind {
        Kind::Iban => detect::valid_iban(v),
        Kind::Swift => detect::valid_swift(v),
        Kind::Crypto => detect::valid_crypto(v),
        Kind::Cusip => detect::valid_cusip(v),
        Kind::Isin => detect::valid_isin(v),
        Kind::Account => detect::valid_account(v),
        Kind::Sedol => detect::valid_sedol(v),
        Kind::PanSad => false,
    }
}

/// Every hit in the tables of `text` for the wanted `kinds`.
pub fn scan_tables(text: &str, kinds: &[Kind]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let all = columnar::lines(text);
    let want = |k: Kind| kinds.contains(&k);
    let mut i = 0;
    while i < all.len() {
        let Some((delim, map, width)) = header(&all[i], &want) else {
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

/// The delimiter, the kind of each column and the column count when `line` is a table header.
#[allow(clippy::type_complexity)]
fn header(line: &Line<'_>, want: &dyn Fn(Kind) -> bool) -> Option<(u8, Vec<Option<Kind>>, usize)> {
    for delim in DELIMITERS {
        let cells = columnar::split_cells(line.text, delim, 0);
        if cells.len() < 2 {
            continue;
        }
        let map: Vec<Option<Kind>> = cells
            .iter()
            .map(|(s, e)| column_kind(&line.text[*s..*e]).filter(|k| want(*k)))
            .collect();
        if map.iter().any(Option::is_some) {
            return Some((delim, map, cells.len()));
        }
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
            .take(4096)
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

    fn found(text: &str) -> Vec<Kind> {
        let mut kinds: Vec<Kind> = scan(text, &Kind::DEFAULT)
            .into_iter()
            .map(|h| h.kind)
            .collect();
        kinds.sort();
        kinds
    }

    #[test]
    fn headers_give_bare_cells_their_meaning() {
        assert_eq!(found("id,iban\n1,GB82WEST12345698765432\n"), [Kind::Iban]);
        assert_eq!(
            found("id,bic\n1,DEUTDEFF\n"),
            [Kind::Swift],
            "a header is context for a BIC"
        );
        assert_eq!(
            found("cusip\tisin\n037833100\tUS0378331005\n"),
            [Kind::Cusip, Kind::Isin]
        );
        assert_eq!(
            found("wallet|note\n0x52908400098527886E0F7030069857D2E4169EE7|x\n"),
            [Kind::Crypto]
        );
    }

    #[test]
    fn quoted_cells_and_other_delimiters_work() {
        assert_eq!(
            found("\"iban\",note\n\"GB82 WEST 1234 5698 7654 32\",x\n"),
            [Kind::Iban]
        );
        assert_eq!(
            found("| isin | note |\n|---|---|\n| US0378331005 | x |\n"),
            [Kind::Isin]
        );
    }

    #[test]
    fn sql_and_copy_blocks_map_values_to_columns() {
        let sql =
            "INSERT INTO t (id, iban, swift) VALUES (1, 'GB82WEST12345698765432', 'DEUTDEFF'), (2, 'N/A', NULL);";
        assert_eq!(found(sql), [Kind::Iban, Kind::Swift]);
        let copy = "COPY t (id, isin) FROM stdin;\n1\tUS0378331005\n\\.\n";
        assert_eq!(found(copy), [Kind::Isin]);
    }

    #[test]
    fn invalid_cells_are_not_reported() {
        assert!(found("iban\nGB82WEST12345698765433\n").is_empty());
        assert!(found("id,price,qty\n1,9.99,3\n").is_empty());
        assert!(found("iban,isin\nN/A,N/A\n").is_empty());
        assert!(found("INSERT INTO t (a, b) VALUES (1, 2);").is_empty());
    }

    fn table_values(text: &str) -> Vec<&str> {
        super::scan_tables(text, &Kind::DEFAULT)
            .into_iter()
            .map(|h| &text[h.start..h.end])
            .collect()
    }

    #[test]
    fn a_ragged_row_does_not_end_the_table() {
        let iban = "GB82WEST12345698765432";
        assert_eq!(
            table_values(&format!("iban,note\n{iban},x\nfoo,bar,baz\n{iban},y\n")),
            [iban, iban]
        );
        assert_eq!(
            table_values(&format!("iban,note\n{iban},\"a\nb\"\n{iban},y\n")),
            [iban, iban],
            "a quoted cell spanning lines leaves a short line behind"
        );
    }

    #[test]
    fn a_normal_table_is_read_in_full() {
        let iban = "GB82WEST12345698765432";
        assert_eq!(
            table_values(&format!("iban,note\n{iban},x\n{iban},y\n")),
            [iban, iban]
        );
    }

    #[test]
    fn a_blank_line_still_ends_the_table() {
        let iban = "GB82WEST12345698765432";
        assert_eq!(
            table_values(&format!("iban,note\n{iban},x\n\n{iban},y\n")),
            [iban]
        );
    }

    #[test]
    fn a_cell_is_reported_once() {
        let hits = scan("iban\nGB82WEST12345698765432\n", &Kind::DEFAULT);
        assert_eq!(hits.len(), 1);
    }
}
