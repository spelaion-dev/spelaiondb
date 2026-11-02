use std::fmt;

/// A secret (password, passphrase…). Never printed: `Debug` and `Display` show `***`.
/// Using it requires asking explicitly with [`Secret::expose`].
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_prints_the_value() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?} {s}"), "*** ***");
        assert_eq!(s.expose(), "hunter2");
    }
}
