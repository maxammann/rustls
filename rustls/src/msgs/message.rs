use crate::error::Error;
use crate::msgs::alert::AlertMessagePayload;
use crate::msgs::base::Payload;
use crate::msgs::ccs::ChangeCipherSpecPayload;
use crate::msgs::codec::{Codec, Reader};
use crate::msgs::enums::HandshakeType;
use crate::msgs::enums::{AlertDescription, AlertLevel};
use crate::msgs::enums::{ContentType, ProtocolVersion};
use crate::msgs::handshake::HandshakeMessagePayload;

use std::convert::TryFrom;
use crate::msgs::heartbeat::HeartbeatPayload;

#[derive(Debug, Clone)]
pub enum MessagePayload {
    Alert(AlertMessagePayload),
    Handshake(HandshakeMessagePayload),
    // this type is for TLS 1.2 encrypted handshake messages
    TLS12EncryptedHandshake(Payload),
    ChangeCipherSpec(ChangeCipherSpecPayload),
    ApplicationData(Payload),
    Heartbeat(HeartbeatPayload),
}

impl MessagePayload {
    pub fn encode(&self, bytes: &mut Vec<u8>) {
        match *self {
            MessagePayload::Alert(ref x) => x.encode(bytes),
            MessagePayload::Handshake(ref x) => x.encode(bytes),
            MessagePayload::TLS12EncryptedHandshake(ref x) => x.encode(bytes),
            MessagePayload::ChangeCipherSpec(ref x) => x.encode(bytes),
            MessagePayload::ApplicationData(ref x) => x.encode(bytes),
            MessagePayload::Heartbeat(ref x) => x.encode(bytes),
        }
    }

    pub fn new(
        typ: ContentType,
        vers: ProtocolVersion,
        payload: Payload,
    ) -> Result<MessagePayload, Error> {
        let fallback_payload = payload.clone();
        let mut r = Reader::init(&payload.0);
        let parsed = match typ {
            ContentType::ApplicationData => return Ok(MessagePayload::ApplicationData(payload)),
            ContentType::Alert => AlertMessagePayload::read(&mut r).map(MessagePayload::Alert),
            ContentType::Handshake => {
                HandshakeMessagePayload::read_version(&mut r, vers).map(MessagePayload::Handshake)
                    // this type is for TLS 1.2 encrypted handshake messages
                    .or(Some(MessagePayload::TLS12EncryptedHandshake(fallback_payload)))
            }
            ContentType::ChangeCipherSpec => {
                ChangeCipherSpecPayload::read(&mut r).map(MessagePayload::ChangeCipherSpec)
            }
            ContentType::Heartbeat => {
                HeartbeatPayload::read(&mut r).map(MessagePayload::Heartbeat)
            }
            _ => None,
        };

        parsed.ok_or(Error::CorruptMessagePayload(typ))
        // Ignore unused appended data
        /*parsed
            .filter(|_| !r.any_left())
            .ok_or(Error::CorruptMessagePayload(typ))*/
    }

    pub fn content_type(&self) -> ContentType {
        match self {
            MessagePayload::Alert(_) => ContentType::Alert,
            MessagePayload::Handshake(_) => ContentType::Handshake,
            MessagePayload::TLS12EncryptedHandshake(_) => ContentType::Handshake,
            MessagePayload::ChangeCipherSpec(_) => ContentType::ChangeCipherSpec,
            MessagePayload::ApplicationData(_) => ContentType::ApplicationData,
            MessagePayload::Heartbeat(_) => ContentType::Heartbeat,
        }
    }
}

/// A TLS frame, named TLSPlaintext in the standard.
///
/// This type owns all memory for its interior parts. It is used to read/write from/to I/O
/// buffers as well as for fragmenting, joining and encryption/decryption. It can be converted
/// into a `Message` by decoding the payload.
#[derive(Clone, Debug)]
pub struct OpaqueMessage {
    pub typ: ContentType,
    pub version: ProtocolVersion,
    pub payload: Payload,
}

impl OpaqueMessage {
    /// `MessageError` allows callers to distinguish between valid prefixes (might
    /// become valid if we read more data) and invalid data.
    pub fn read(r: &mut Reader) -> Result<OpaqueMessage, MessageError> {
        let typ = ContentType::read(r).ok_or(MessageError::TooShortForHeader)?;
        let version = ProtocolVersion::read(r).ok_or(MessageError::TooShortForHeader)?;
        let len = u16::read(r).ok_or(MessageError::TooShortForHeader)?;

        // Reject oversize messages
        if len >= Self::MAX_PAYLOAD {
            return Err(MessageError::IllegalLength);
        }

        // Don't accept any new content-types.
        if let ContentType::Unknown(_) = typ {
            return Err(MessageError::IllegalContentType);
        }

        // Accept only versions 0x03XX for any XX.
        match version {
            ProtocolVersion::Unknown(ref v) if (v & 0xff00) != 0x0300 => {
                return Err(MessageError::IllegalProtocolVersion);
            }
            _ => {}
        };

        let mut sub = r
            .sub(len as usize)
            .ok_or(MessageError::TooShortForLength)?;
        let payload = Payload::read(&mut sub);

        Ok(OpaqueMessage {
            typ,
            version,
            payload,
        })
    }

    pub fn encode(self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.typ.encode(&mut buf);
        self.version.encode(&mut buf);
        (self.payload.0.len() as u16).encode(&mut buf);
        self.payload.encode(&mut buf);
        buf
    }

    pub fn borrow(&self) -> BorrowedOpaqueMessage<'_> {
        BorrowedOpaqueMessage {
            typ: self.typ,
            version: self.version,
            payload: &self.payload.0,
        }
    }

    /// This is the maximum on-the-wire size of a TLSCiphertext.
    /// That's 2^14 payload bytes, a header, and a 2KB allowance
    /// for ciphertext overheads.
    const MAX_PAYLOAD: u16 = 16384 + 2048;

    /// Content type, version and size.
    const HEADER_SIZE: u16 = 1 + 2 + 2;

    /// Maximum on-wire message size.
    pub const MAX_WIRE_SIZE: usize = (Self::MAX_PAYLOAD + Self::HEADER_SIZE) as usize;
}

impl From<Message> for OpaqueMessage {
    fn from(msg: Message) -> OpaqueMessage {
        let typ = msg.payload.content_type();
        let payload = match msg.payload {
            MessagePayload::ApplicationData(payload) => payload,
            _ => {
                let mut buf = Vec::new();
                msg.payload.encode(&mut buf);
                Payload(buf)
            }
        };

        OpaqueMessage {
            typ,
            version: msg.version,
            payload,
        }
    }
}

/// A message with decoded payload
#[derive(Debug, Clone)]
pub struct Message {
    pub version: ProtocolVersion,
    pub payload: MessagePayload,
}

impl Message {
    pub fn is_handshake_type(&self, hstyp: HandshakeType) -> bool {
        // Bit of a layering violation, but OK.
        if let MessagePayload::Handshake(ref hsp) = self.payload {
            hsp.typ == hstyp
        } else {
            false
        }
    }

    pub fn build_alert(level: AlertLevel, desc: AlertDescription) -> Message {
        Message {
            version: ProtocolVersion::TLSv1_2,
            payload: MessagePayload::Alert(AlertMessagePayload {
                level,
                description: desc,
            }),
        }
    }

    pub fn build_key_update_notify() -> Message {
        Message {
            version: ProtocolVersion::TLSv1_3,
            payload: MessagePayload::Handshake(HandshakeMessagePayload::build_key_update_notify()),
        }
    }
}

impl TryFrom<OpaqueMessage> for Message {
    type Error = Error;

    fn try_from(opaque: OpaqueMessage) -> Result<Self, Self::Error> {
        Ok(Message {
            version: opaque.version,
            payload: MessagePayload::new(opaque.typ, opaque.version, opaque.payload)?,
        })
    }
}

/// A TLS frame, named TLSPlaintext in the standard.
///
/// This type differs from `OpaqueMessage` because it borrows
/// its payload.  You can make a `OpaqueMessage` from an
/// `BorrowMessage`, but this involves a copy.
///
/// This type also cannot decode its internals and
/// cannot be read/encoded; only `OpaqueMessage` can do that.
pub struct BorrowedOpaqueMessage<'a> {
    pub typ: ContentType,
    pub version: ProtocolVersion,
    pub payload: &'a [u8],
}

#[derive(Debug)]
pub enum MessageError {
    TooShortForHeader,
    TooShortForLength,
    IllegalLength,
    IllegalContentType,
    IllegalProtocolVersion,
}
