use std::io;
use std::result;
use std::error::Error;

use snap::Error as SnapError;

#[derive(Debug)]
pub enum StatusCode {
    NotFound,
    IOError,
    ChecksumError,
    SnapError,
    CompressError,
    InvalidData,
}

#[derive(Debug)]
pub struct Status {
    pub code: StatusCode,
    pub msg: String,
}

impl Status {
    fn new(code: StatusCode, msg: &str) -> Self {
        let msg = if msg.is_empty() {
            format!("{:?}", code)
        } else {
            format!("{:?}: {}", code, msg)
        };
        Status {
            code, msg
        }
    }
}

impl From<io::Error> for Status {
    fn from(e: io::Error) -> Self {
        let code = match e.kind() {
            io::ErrorKind::NotFound => StatusCode::NotFound,
            _ => StatusCode::IOError,
        };
        Status::new(code, e.description())
    }
}

impl From<SnapError> for Status {
    fn from(e: SnapError) -> Self {
        let code = match e {
            SnapError::Checksum { .. } => StatusCode::ChecksumError,
            _ => StatusCode::SnapError,
        };
        Status::new(code, e.description())
    }
}

pub type MyResult<T> = result::Result<T, Status>;

macro_rules! err {
    ($code:expr, $msg:expr) => {Err($crate::result::Status { code: $code, msg: $msg.to_string() })};
}