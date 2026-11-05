//! What the explorer does with an object besides reading it (ADR-026): its name as the engine
//! writes it, for the clipboard.

use spelaion_driver_api::Dialect;

/// A name as the engine writes it in SQL: `schema.table`, `schema.table.column`…, each part
/// quoted only where the engine needs it. SQLite's and DuckDB's `main` schema is left out, as
/// in the SQL the explorer generates; an empty schema means a name outside every schema (a
/// schema itself).
pub fn qualified_name(dialect: &dyn Dialect, schema: &str, names: &[String]) -> String {
    let main =
        matches!(dialect.name(), "sqlite" | "duckdb") && schema == "main" && !names.is_empty();
    let schema = (!schema.is_empty() && !main).then_some(schema);
    schema
        .into_iter()
        .chain(names.iter().map(String::as_str))
        .map(|part| dialect.quote_identifier(part))
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_duckdb::DuckdbDialect;
    use spelaion_driver_mysql::MysqlDialect;
    use spelaion_driver_postgres::PostgresDialect;
    use spelaion_driver_sqlite::SqliteDialect;
    use spelaion_driver_sqlserver::SqlServerDialect;

    fn names(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn qualified_names_quote_each_part_as_the_engine_does() {
        let pg = PostgresDialect;
        assert_eq!(
            qualified_name(&pg, "public", &names(&["users"])),
            "public.users"
        );
        assert_eq!(
            qualified_name(&pg, "Sales", &names(&["Order Lines", "qty"])),
            "\"Sales\".\"Order Lines\".qty"
        );
        // A part that holds the quote character cannot end the quoting early.
        assert_eq!(qualified_name(&pg, "", &names(&["a\"b"])), "\"a\"\"b\"");
        assert_eq!(
            qualified_name(&MysqlDialect, "shop", &names(&["orders"])),
            "`shop`.`orders`"
        );
        assert_eq!(
            qualified_name(&SqlServerDialect, "dbo", &names(&["Order]s"])),
            "dbo.[Order]]s]"
        );
    }

    #[test]
    fn main_is_left_out_where_the_engine_has_it() {
        let engines: [&dyn Dialect; 2] = [&SqliteDialect, &DuckdbDialect];
        for dialect in engines {
            assert_eq!(qualified_name(dialect, "main", &names(&["t", "c"])), "t.c");
            // The schema itself keeps its name.
            assert_eq!(qualified_name(dialect, "", &names(&["main"])), "main");
            assert_eq!(qualified_name(dialect, "other", &names(&["t"])), "other.t");
        }
        assert_eq!(
            qualified_name(&PostgresDialect, "main", &names(&["t"])),
            "main.t"
        );
    }
}
