//! Parsing a config line into a typed setting.
//!
//! `parse_setting` does not compile: it was changed to report *why* a line was
//! rejected, and the return type was left behind.

/// Why a line could not be read as a setting.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
    Empty,
    NoEquals,
    NoKey,
}

/// One `key = value` line, with the surrounding space trimmed off both halves.
#[derive(Debug, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    pub value: String,
}

/// Read one line as a setting.
pub fn parse_setting(line: &str) -> Option<Setting> {
    let line = line.trim();
    if line.is_empty() {
        return Err(Invalid::Empty);
    }
    let Some((key, value)) = line.split_once('=') else {
        return Err(Invalid::NoEquals);
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(Invalid::NoKey);
    }
    Ok(Setting {
        key: key.to_string(),
        value: value.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_line_parses() {
        assert_eq!(
            parse_setting("  model = opus  "),
            Ok(Setting {
                key: "model".to_string(),
                value: "opus".to_string(),
            })
        );
    }

    #[test]
    fn each_rejection_says_why() {
        assert_eq!(parse_setting("   "), Err(Invalid::Empty));
        assert_eq!(parse_setting("no equals here"), Err(Invalid::NoEquals));
        assert_eq!(parse_setting("  = orphan"), Err(Invalid::NoKey));
    }
}
