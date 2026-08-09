use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Config(String),
    Database(turso::Error),
    Http(reqwest::Error),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => formatter.write_str(message),
            Self::Database(err) => write!(formatter, "Turso database error: {err}"),
            Self::Http(err) => write!(formatter, "HTTP request failed: {err}"),
            Self::Io(err) => write!(formatter, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<turso::Error> for Error {
    fn from(err: turso::Error) -> Self {
        Self::Database(err)
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
