use std::ffi::OsStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameValidationError {
    Empty,
    DotName,
    InvalidCharacter(char),
    TrailingSpaceOrDot,
    ReservedName,
}

pub fn validate_name(name: &OsStr) -> Result<(), NameValidationError> {
    if name.is_empty() {
        return Err(NameValidationError::Empty);
    }
    let text = name.to_string_lossy();
    if text == "." || text == ".." {
        return Err(NameValidationError::DotName);
    }
    if text.ends_with(' ') || text.ends_with('.') {
        return Err(NameValidationError::TrailingSpaceOrDot);
    }
    if let Some(character) = text.chars().find(|character| {
        matches!(
            character,
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
        ) || *character < ' '
    }) {
        return Err(NameValidationError::InvalidCharacter(character));
    }
    let stem = text.split('.').next().unwrap_or_default();
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || upper.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        });
    if reserved {
        return Err(NameValidationError::ReservedName);
    }
    Ok(())
}
