use std::fmt;

use bytes::{Buf, BufMut};

use crate::coding::{self, BufExt, BufMutExt};
use crate::frame;

/// Transport-level errors occur when a peer violates the protocol specification
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Error {
    /// Type of error
    pub code: Code,
    /// Frame type that triggered the error
    pub frame: Option<frame::Type>,
    /// Human-readable explanation of the reason
    pub reason: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.code.fmt(f)?;
        if let Some(frame) = self.frame {
            write!(f, " in {}", frame)?;
        }
        if !self.reason.is_empty() {
            write!(f, ": {}", self.reason)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<Code> for Error {
    fn from(x: Code) -> Self {
        Self {
            code: x,
            frame: None,
            reason: "".to_string(),
        }
    }
}

/// Transport-level error code
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Code(u64);

impl Code {
    /// Create QUIC error code from TLS alert code
    pub(crate) fn crypto(code: u8) -> Self {
        Code(0x100 | u64::from(code))
    }
}

impl coding::Codec for Code {
    fn decode<B: Buf>(buf: &mut B) -> coding::Result<Self> {
        Ok(Code(buf.get_var()?))
    }
    fn encode<B: BufMut>(&self, buf: &mut B) {
        buf.write_var(self.0)
    }
}

impl From<Code> for u64 {
    fn from(x: Code) -> u64 {
        x.0
    }
}

macro_rules! errors {
    {$($name:ident($val:expr) $desc:expr;)*} => {
        #[allow(non_snake_case, unused)]
        impl Error {
            $(
            pub(crate) fn $name<T>(reason: T) -> Self where T: Into<String> {
                Self {
                    code: Code::$name,
                    frame: None,
                    reason: reason.into(),
                }
            }
            )*
        }

        impl Code {
            $(#[doc = $desc] pub const $name: Self = Code($val);)*
        }

        impl fmt::Debug for Code {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.0 {
                    $($val => f.write_str(stringify!($name)),)*
                    x if x >= 0x100 && x < 0x200 => write!(f, "Code::crypto({:02x})", self.0 as u8),
                    _ => write!(f, "Code({:x})", self.0),
                }
            }
        }

        impl fmt::Display for Code {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let x = match self.0 {
                    $($val => $desc,)*
                    _ if self.0 >= 0x100 && self.0 < 0x200 => "the cryptographic handshake failed", // FIXME: Describe specific alert
                    _ => "unknown error",
                };
                f.write_str(x)
            }
        }
    }
}

errors! {
    NO_ERROR(0x0) "the connection is being closed abruptly in the absence of any error";
    INTERNAL_ERROR(0x1) "the endpoint encountered an internal error and cannot continue with the connection";
    SERVER_BUSY(0x2) "the server is currently busy and does not accept any new connections";
    FLOW_CONTROL_ERROR(0x3) "received more data than permitted in advertised data limits";
    STREAM_LIMIT_ERROR(0x4) "received a frame for a stream identifier that exceeded advertised the stream limit for the corresponding stream type";
    STREAM_STATE_ERROR(0x5) "received a frame for a stream that was not in a state that permitted that frame";
    FINAL_OFFSET_ERROR(0x6) "received a STREAM frame containing data that exceeded the previously established final offset, or a RST_STREAM frame containing a final offset that was lower than the maximum offset of data that was already received, or a RST_STREAM frame containing a different final offset to the one already established";
    FRAME_ENCODING_ERROR(0x7) "received a frame that was badly formatted";
    TRANSPORT_PARAMETER_ERROR(0x8) "received transport parameters that were badly formatted, included an invalid value, was absent even though it is mandatory, was present though it is forbidden, or is otherwise in error";
    VERSION_NEGOTIATION_ERROR(0x9) "received transport parameters that contained version negotiation parameters that disagreed with the version negotiation that was performed, constituting a potential version downgrade attack";
    PROTOCOL_VIOLATION(0xA) "detected an error with protocol compliance that was not covered by more specific error codes";
    CRYPTO_BUFFER_EXCEEDED(0xD) "received more data in CRYPTO frames than can be buffered";
}
