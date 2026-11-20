//! Finds financial identifiers in tables: delimited text with a header row, SQL
//! `INSERT ... (cols) VALUES` and `COPY ... FROM stdin` blocks.
//!
//! A header names a column, and every cell under it is then checked as that kind of identifier,
//! so a bare account number under `account` is found even though nothing next to it says so.

use crate::detect::{self, Hit, Kind};
use reiki_detect::columnar;

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
    columnar::scan_tables(
        text,
        &|k| kinds.contains(&k),
        columnar::TableRules {
            column_kind,
            cell_ok,
            catch_all: None,
        },
    )
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
