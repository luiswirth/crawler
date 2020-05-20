use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),

    #[error("{0}")]
    Fern(#[from] fern::InitError),

    #[error("{0}")]
    Reqwest(#[from] reqwest::Error),
}
