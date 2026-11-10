//! The name of the file a save dialog answers, kept in step with the file type the user chose.
//! GTK's dialog (Linux) neither renames the file when another type is picked nor adds the
//! type's extension to a name typed without one, so the app does both with these.

/// The extension a dialog's file type stands for: its first one; none for "All files" (`*`).
pub fn type_extension(extensions: &[String]) -> Option<&str> {
    extensions
        .first()
        .map(String::as_str)
        .filter(|e| !e.is_empty() && *e != "*")
}

/// `name` split into its stem and its extension, if it has one: `.bashrc` and `name.` have none.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            (stem, Some(extension))
        }
        _ => (name, None),
    }
}

/// `name` with `extension` added, without doubling a trailing dot (`name.` becomes `name.sql`).
fn add_extension(name: &str, extension: &str) -> String {
    format!("{}.{extension}", name.strip_suffix('.').unwrap_or(name))
}

/// The name a save dialog shows once the user picks the type with `extension`. A name without
/// an extension, or with the extension of another of the dialog's types (`known`), takes the
/// type's; any other extension was typed on purpose and stays. `None` when nothing changes.
/// Extensions are compared without case.
pub fn renamed_for_type(name: &str, extension: &str, known: &[&str]) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    match split_extension(name) {
        (_, None) => Some(add_extension(name, extension)),
        (_, Some(current)) if current.eq_ignore_ascii_case(extension) => None,
        (stem, Some(current)) if known.iter().any(|k| k.eq_ignore_ascii_case(current)) => {
            Some(format!("{stem}.{extension}"))
        }
        _ => None,
    }
}

/// The glob pattern that lists a type's files in GTK's dialog, which compares patterns with
/// case: `sql` becomes `*.[sS][qQ][lL]`, so `REPORT.SQL` is listed too; `*` stands for every file.
pub fn type_pattern(extension: &str) -> String {
    if extension.is_empty() || extension == "*" {
        return "*".to_string();
    }
    let mut pattern = String::from("*.");
    for c in extension.chars() {
        let (lower, upper) = (c.to_ascii_lowercase(), c.to_ascii_uppercase());
        if lower == upper {
            pattern.push(c);
        } else {
            pattern.extend(['[', lower, upper, ']']);
        }
    }
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHART: &[&str] = &["png", "svg"];

    #[test]
    fn a_type_stands_for_its_first_extension_and_all_files_for_none() {
        let png = vec!["png".to_string(), "PNG".to_string()];
        assert_eq!(type_extension(&png), Some("png"));
        assert_eq!(type_extension(&["*".to_string()]), None);
        assert_eq!(type_extension(&[]), None);
        assert_eq!(type_extension(&[String::new()]), None);
    }

    #[test]
    fn picking_another_type_swaps_an_extension_of_the_dialog() {
        assert_eq!(
            renamed_for_type("chart.png", "svg", CHART).as_deref(),
            Some("chart.svg")
        );
        assert_eq!(
            renamed_for_type("chart.PNG", "svg", CHART).as_deref(),
            Some("chart.svg")
        );
        assert_eq!(
            renamed_for_type("q3.sales.svg", "png", CHART).as_deref(),
            Some("q3.sales.png")
        );
    }

    #[test]
    fn picking_a_type_gives_a_bare_name_its_extension() {
        assert_eq!(
            renamed_for_type("chart", "svg", CHART).as_deref(),
            Some("chart.svg")
        );
        assert_eq!(
            renamed_for_type("chart.", "svg", CHART).as_deref(),
            Some("chart.svg")
        );
        assert_eq!(
            renamed_for_type(".chart", "svg", CHART).as_deref(),
            Some(".chart.svg")
        );
    }

    #[test]
    fn picking_a_type_keeps_a_name_that_already_fits_or_was_typed_on_purpose() {
        assert_eq!(renamed_for_type("chart.svg", "svg", CHART), None);
        assert_eq!(renamed_for_type("chart.SVG", "svg", CHART), None);
        assert_eq!(renamed_for_type("chart.backup", "svg", CHART), None);
        assert_eq!(renamed_for_type("", "svg", CHART), None);
    }

    #[test]
    fn a_type_lists_its_files_whatever_their_case() {
        assert_eq!(type_pattern("sql"), "*.[sS][qQ][lL]");
        assert_eq!(type_pattern("mp4"), "*.[mM][pP]4");
        assert_eq!(type_pattern("*"), "*");
        assert_eq!(type_pattern(""), "*");
    }
}
