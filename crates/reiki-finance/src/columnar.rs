//! Finds financial identifiers in tables: delimited text with a header row, SQL
//! `INSERT ... (cols) VALUES` and `COPY ... FROM stdin` blocks.
//!
//! A header names a column, and every cell under it is then checked as that kind of identifier,
//! so a bare account number under `account` is found even though nothing next to it says so.

use crate::detect::{self, Hit, Kind};

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

/// The `(start, end)` byte range of each cell in `line`, offset by `base`; quotes are excluded.
fn split_cells(line: &str, delim: u8, base: usize) -> Vec<(usize, usize)> {
    let b = line.as_bytes();
    let mut cells = Vec::new();
    let mut i = 0;
    loop {
        let mut start = i;
        while b.get(start) == Some(&b' ') && delim != b' ' {
            start += 1;
        }
        let (s, e, next);
        if b.get(start) == Some(&b'"') {
            let mut j = start + 1;
            loop {
                match b.get(j) {
                    None => break,
                    Some(b'"') if b.get(j + 1) == Some(&b'"') => j += 2,
                    Some(b'"') => break,
                    Some(_) => j += 1,
                }
            }
            s = start + 1;
            e = j.min(b.len());
            let after = (j + 1).min(b.len());
            next = b[after..]
                .iter()
                .position(|c| *c == delim)
                .map(|p| after + p);
        } else {
            let end = b[i..]
                .iter()
                .position(|c| *c == delim)
                .map_or(b.len(), |p| i + p);
            s = start.min(end);
            let mut trimmed = end;
            while trimmed > s && b[trimmed - 1] == b' ' {
                trimmed -= 1;
            }
            e = trimmed;
            next = (end < b.len()).then_some(end);
        }
        cells.push((base + s, base + e));
        match next {
            Some(p) => i = p + 1,
            None => break,
        }
    }
    cells
}

struct Line<'a> {
    start: usize,
    text: &'a str,
}

fn lines(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    for piece in text.split_inclusive('\n') {
        out.push(Line {
            start,
            text: piece.trim_end_matches(['\r', '\n']),
        });
        start += piece.len();
    }
    out
}

/// Every hit in the tables of `text` for the wanted `kinds`.
pub fn scan_tables(text: &str, kinds: &[Kind]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let all = lines(text);
    let want = |k: Kind| kinds.contains(&k);
    let mut i = 0;
    while i < all.len() {
        let Some((delim, map, width)) = header(&all[i], &want) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < all.len() {
            let cells = split_cells(all[j].text, delim, all[j].start);
            if cells.len() != width {
                break;
            }
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
            j += 1;
        }
        i = j;
    }
    sql(text, &want, &mut hits);
    copy_blocks(text, &all, &want, &mut hits);
    hits
}

/// The delimiter, the kind of each column and the column count when `line` is a table header.
#[allow(clippy::type_complexity)]
fn header(line: &Line<'_>, want: &dyn Fn(Kind) -> bool) -> Option<(u8, Vec<Option<Kind>>, usize)> {
    for delim in DELIMITERS {
        let cells = split_cells(line.text, delim, 0);
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
        i = tuples(text, i, &map, hits);
        from = from.max(i);
    }
}

/// Reads the `(...)` tuples after VALUES starting at `i`; returns where it stopped.
fn tuples(text: &str, mut i: usize, map: &[Option<Kind>], hits: &mut Vec<Hit>) -> usize {
    let b = text.as_bytes();
    loop {
        while b
            .get(i)
            .is_some_and(|c| c.is_ascii_whitespace() || *c == b',')
        {
            i += 1;
        }
        if b.get(i) != Some(&b'(') {
            return i;
        }
        i += 1;
        let mut column = 0;
        loop {
            while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            let (s, e, next) = if b.get(i) == Some(&b'\'') {
                let mut j = i + 1;
                loop {
                    match b.get(j) {
                        None => break,
                        Some(b'\'') if b.get(j + 1) == Some(&b'\'') => j += 2,
                        Some(b'\'') => break,
                        Some(_) => j += 1,
                    }
                }
                (i + 1, j.min(b.len()), (j + 1).min(b.len()))
            } else {
                let mut j = i;
                let mut depth = 0usize;
                while let Some(c) = b.get(j) {
                    match c {
                        b'(' => depth += 1,
                        b')' if depth == 0 => break,
                        b')' => depth -= 1,
                        b',' if depth == 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                let mut end = j;
                while end > i && b[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                (i, end, j)
            };
            if let Some(Some(kind)) = map.get(column) {
                if cell_ok(*kind, &text[s..e]) {
                    hits.push(Hit {
                        kind: *kind,
                        start: s,
                        end: e,
                    });
                }
            }
            column += 1;
            i = next;
            while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            match b.get(i) {
                Some(b',') => i += 1,
                Some(b')') => {
                    i += 1;
                    break;
                }
                _ => return i,
            }
        }
    }
}

/// `COPY t (a, b) FROM stdin;` followed by tab-separated rows up to `\.`.
fn copy_blocks(text: &str, all: &[Line<'_>], want: &dyn Fn(Kind) -> bool, hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i < all.len() {
        let lower = all[i].text.to_ascii_lowercase();
        let head = lower.trim_start();
        let (Some(open), Some(close)) = (all[i].text.find('('), all[i].text.find(')')) else {
            i += 1;
            continue;
        };
        if !(head.starts_with("copy ") && head.contains("from stdin") && open < close) {
            i += 1;
            continue;
        }
        let map: Vec<Option<Kind>> = all[i].text[open + 1..close]
            .split(',')
            .map(|name| column_kind(name.trim().trim_matches('"')).filter(|k| want(*k)))
            .collect();
        let mut j = i + 1;
        while j < all.len() && all[j].text != "\\." {
            let cells = split_cells(all[j].text, b'\t', all[j].start);
            if cells.len() == map.len() {
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
        i = j + 1;
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

    #[test]
    fn a_cell_is_reported_once() {
        let hits = scan("iban\nGB82WEST12345698765432\n", &Kind::DEFAULT);
        assert_eq!(hits.len(), 1);
    }
}
