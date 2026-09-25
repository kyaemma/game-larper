use std::fmt;

#[derive(Debug)]
pub enum Error {
    Format(String),
    Json(serde_json::Error),
    UnsafePath(&'static str),
    InvalidApplicationId,
    Io(std::io::Error),
    Queue(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(message) => formatter.write_str(message),
            Self::Queue(message) => formatter.write_str(message),
            Self::Json(error) => write!(formatter, "The file is not valid JSON: {error}"),
            Self::UnsafePath(message) => formatter.write_str(message),
            Self::InvalidApplicationId => formatter.write_str("Invalid Discord application ID."),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}
