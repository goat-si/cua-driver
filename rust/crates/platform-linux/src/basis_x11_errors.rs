use cua_driver_core::guarded_dispatch::Emitted;
use x11rb::errors::{ConnectionError, ReplyError};

pub(crate) fn request_error(error: &ConnectionError) -> Emitted {
    match error {
        ConnectionError::UnsupportedExtension | ConnectionError::MaximumRequestLengthExceeded => {
            Emitted::NotEmitted
        }
        ConnectionError::UnknownError
        | ConnectionError::FdPassingFailed
        | ConnectionError::ParseError(_)
        | ConnectionError::InsufficientMemory
        | ConnectionError::IoError(_) => Emitted::Unknown,
        _ => Emitted::Unknown,
    }
}
pub(crate) fn reply_error(error: &ReplyError) -> Emitted {
    match error {
        ReplyError::X11Error(_) => Emitted::NotEmitted,
        ReplyError::ConnectionError(_) => Emitted::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::errors::ParseError;

    #[test]
    fn request_creation_errors_are_classified() {
        for (error, expected) in [
            (ConnectionError::UnsupportedExtension, Emitted::NotEmitted),
            (
                ConnectionError::MaximumRequestLengthExceeded,
                Emitted::NotEmitted,
            ),
            (ConnectionError::UnknownError, Emitted::Unknown),
            (ConnectionError::FdPassingFailed, Emitted::Unknown),
            (
                ConnectionError::ParseError(ParseError::InsufficientData),
                Emitted::Unknown,
            ),
            (ConnectionError::InsufficientMemory, Emitted::Unknown),
            (
                ConnectionError::IoError(std::io::Error::other("lost")),
                Emitted::Unknown,
            ),
        ] {
            assert_eq!(request_error(&error), expected, "{error:?}");
        }
    }

    #[test]
    fn checked_reply_errors_are_classified_by_stage() {
        let server = x11rb::x11_utils::X11Error {
            error_kind: x11rb::protocol::ErrorKind::Window,
            error_code: 3,
            sequence: 1,
            bad_value: 0,
            minor_opcode: 0,
            major_opcode: 0,
            extension_name: None,
            request_name: None,
        };
        assert_eq!(
            reply_error(&ReplyError::X11Error(server)),
            Emitted::NotEmitted
        );
        for error in [
            ConnectionError::UnsupportedExtension,
            ConnectionError::UnknownError,
        ] {
            assert_eq!(
                reply_error(&ReplyError::ConnectionError(error)),
                Emitted::Unknown
            );
        }
    }
}
