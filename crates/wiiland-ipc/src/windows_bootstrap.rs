//! Fixed-width wire contract for the Windows B/R pipe bootstrap.
//!
//! These records establish which private return pipe participates in a daemon
//! connection. They do not authenticate either endpoint; the Windows client and
//! daemon must independently impersonate each connected writer and validate its
//! token before accepting the next record. Identifier bytes are public routing
//! material, not a secret or an authentication credential.

use std::{error::Error, fmt, time::Duration};

/// Bootstrap transport version, independent of the JSON IPC protocol version.
pub const TRANSPORT_VERSION: u32 = 1;
/// Only successful bootstrap status. Any nonzero status indicates generic failure.
pub const READY_STATUS_OK: u32 = 0;
/// Access granted to the current logon on both bootstrap pipe endpoints.
/// This is `FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES |
/// READ_CONTROL | SYNCHRONIZE`; it excludes `FILE_CREATE_PIPE_INSTANCE`.
pub const BOOTSTRAP_ACCESS_MASK: u32 = 0x0012_0083;
/// Absolute deadline for the entire B/R connection attempt, not per record.
pub const BOOTSTRAP_DEADLINE: Duration = Duration::from_secs(2);

/// Width of the random return-pipe identifier and challenge, in bytes.
pub const NONCE_LEN: usize = 16;
/// Fixed encoded width of a client announce record.
pub const CLIENT_ANNOUNCE_LEN: usize = 4 + NONCE_LEN;
/// Fixed encoded width of a daemon hello record.
pub const DAEMON_HELLO_LEN: usize = 4 + NONCE_LEN;
/// Fixed encoded width of a client echo record.
pub const CLIENT_ECHO_LEN: usize = 4 + NONCE_LEN;
/// Fixed encoded width of a daemon ready record.
pub const DAEMON_READY_LEN: usize = 8;

const RETURN_PIPE_PREFIX: &str = r"\\.\pipe\WiiLand.";

/// A validated 128-bit random identifier for one private return pipe.
///
/// The identifier is public and must not contain or be reused as a secret. The
/// all-zero value is reserved as invalid. Generate it independently from the
/// daemon's challenge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReturnPipeId([u8; NONCE_LEN]);

impl ReturnPipeId {
    /// Construct an identifier from its raw 16-byte wire representation.
    pub fn from_bytes(bytes: [u8; NONCE_LEN]) -> Result<Self, BootstrapError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(BootstrapError::InvalidIdentifier);
        }
        Ok(Self(bytes))
    }

    /// Parse exactly 32 lowercase hexadecimal characters into an identifier.
    pub fn from_hex(hex: &str) -> Result<Self, BootstrapError> {
        let bytes = hex.as_bytes();
        if bytes.len() != NONCE_LEN * 2 {
            return Err(BootstrapError::InvalidIdentifierHex);
        }

        let mut identifier = [0; NONCE_LEN];
        for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let high = lowercase_hex_nibble(pair[0]).ok_or(BootstrapError::InvalidIdentifierHex)?;
            let low = lowercase_hex_nibble(pair[1]).ok_or(BootstrapError::InvalidIdentifierHex)?;
            identifier[index] = (high << 4) | low;
        }
        Self::from_bytes(identifier)
    }

    /// Return the identifier's raw 16-byte representation.
    pub const fn as_bytes(&self) -> &[u8; NONCE_LEN] {
        &self.0
    }

    /// Return the identifier as exactly 32 lowercase hexadecimal characters.
    pub fn to_hex(self) -> String {
        let mut hex = String::with_capacity(NONCE_LEN * 2);
        append_lowercase_hex(&mut hex, &self.0);
        hex
    }
}

/// A syntactically validated Windows logon SID (`S-1-5-5-<u32>-<u32>`).
///
/// This validates the string's shape for safe endpoint naming; it does not
/// establish that the SID belongs to an operating-system token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogonSid<'a>(&'a str);

impl<'a> LogonSid<'a> {
    /// Validate and retain a Windows logon SID string.
    pub fn new(value: &'a str) -> Result<Self, BootstrapError> {
        let mut components = value.split('-');
        let valid = components.next() == Some("S")
            && components.next() == Some("1")
            && components.next() == Some("5")
            && components.next() == Some("5")
            && components.next().is_some_and(is_decimal_u32)
            && components.next().is_some_and(is_decimal_u32)
            && components.next().is_none();
        if !valid {
            return Err(BootstrapError::InvalidLogonSid);
        }
        Ok(Self(value))
    }

    /// Return the validated SID's original string representation.
    pub const fn as_str(&self) -> &'a str {
        self.0
    }
}

/// Derive the only permitted return-pipe namespace from a validated SID and ID.
///
/// The helper always returns `\\.\pipe\WiiLand.<SID>.return.<32 lowercase hex>`;
/// callers cannot provide or append an arbitrary pipe path.
pub fn derive_return_pipe_name(logon_sid: &LogonSid<'_>, identifier: &ReturnPipeId) -> String {
    let mut name = String::with_capacity(RETURN_PIPE_PREFIX.len() + logon_sid.0.len() + 8 + 32);
    name.push_str(RETURN_PIPE_PREFIX);
    name.push_str(logon_sid.0);
    name.push_str(".return.");
    append_lowercase_hex(&mut name, &identifier.0);
    name
}

/// Client announces the private return-pipe identifier over the B pipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientAnnounce {
    identifier: ReturnPipeId,
}

impl ClientAnnounce {
    pub const fn new(identifier: ReturnPipeId) -> Self {
        Self { identifier }
    }

    pub const fn identifier(&self) -> &ReturnPipeId {
        &self.identifier
    }

    /// Encode version-u32 LE followed by the raw 16-byte identifier.
    pub fn encode(self) -> [u8; CLIENT_ANNOUNCE_LEN] {
        let mut record = [0; CLIENT_ANNOUNCE_LEN];
        record[..4].copy_from_slice(&TRANSPORT_VERSION.to_le_bytes());
        record[4..].copy_from_slice(self.identifier.as_bytes());
        record
    }

    /// Parse exactly one announce record and reject a reserved all-zero ID.
    pub fn parse(record: &[u8]) -> Result<Self, BootstrapError> {
        check_record_version::<CLIENT_ANNOUNCE_LEN>(record)?;
        let identifier = record[4..]
            .try_into()
            .expect("validated announce record has a fixed-size identifier");
        Ok(Self::new(ReturnPipeId::from_bytes(identifier)?))
    }
}

/// Daemon sends a fresh challenge over the client's private return pipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonHello {
    challenge: [u8; NONCE_LEN],
}

impl DaemonHello {
    /// The challenge must be generated freshly by the operating system CSPRNG.
    pub const fn new(challenge: [u8; NONCE_LEN]) -> Self {
        Self { challenge }
    }

    pub const fn challenge(&self) -> &[u8; NONCE_LEN] {
        &self.challenge
    }

    /// Encode version-u32 LE followed by the raw 16-byte challenge.
    pub fn encode(self) -> [u8; DAEMON_HELLO_LEN] {
        let mut record = [0; DAEMON_HELLO_LEN];
        record[..4].copy_from_slice(&TRANSPORT_VERSION.to_le_bytes());
        record[4..].copy_from_slice(&self.challenge);
        record
    }

    /// Parse exactly one hello record.
    pub fn parse(record: &[u8]) -> Result<Self, BootstrapError> {
        check_record_version::<DAEMON_HELLO_LEN>(record)?;
        let challenge = record[4..]
            .try_into()
            .expect("validated hello record has a fixed-size challenge");
        Ok(Self::new(challenge))
    }
}

/// Client echoes the daemon challenge over the still-open B pipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientEcho {
    challenge: [u8; NONCE_LEN],
}

impl ClientEcho {
    pub const fn new(challenge: [u8; NONCE_LEN]) -> Self {
        Self { challenge }
    }

    pub const fn challenge(&self) -> &[u8; NONCE_LEN] {
        &self.challenge
    }

    /// Encode version-u32 LE followed by the raw 16-byte challenge.
    pub fn encode(self) -> [u8; CLIENT_ECHO_LEN] {
        let mut record = [0; CLIENT_ECHO_LEN];
        record[..4].copy_from_slice(&TRANSPORT_VERSION.to_le_bytes());
        record[4..].copy_from_slice(&self.challenge);
        record
    }

    /// Parse exactly one echo record.
    pub fn parse(record: &[u8]) -> Result<Self, BootstrapError> {
        check_record_version::<CLIENT_ECHO_LEN>(record)?;
        let challenge = record[4..]
            .try_into()
            .expect("validated echo record has a fixed-size challenge");
        Ok(Self::new(challenge))
    }
}

/// Daemon's final bootstrap result, sent only after both endpoint checks pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonReady {
    status: u32,
}

impl DaemonReady {
    /// Construct a ready record. Only [`READY_STATUS_OK`] indicates success;
    /// nonzero statuses are generic failures and carry no application detail.
    pub const fn new(status: u32) -> Self {
        Self { status }
    }

    pub const fn status(&self) -> u32 {
        self.status
    }

    /// Encode version-u32 LE followed by status-u32 LE.
    pub fn encode(self) -> [u8; DAEMON_READY_LEN] {
        let mut record = [0; DAEMON_READY_LEN];
        record[..4].copy_from_slice(&TRANSPORT_VERSION.to_le_bytes());
        record[4..].copy_from_slice(&self.status.to_le_bytes());
        record
    }

    /// Parse exactly one ready record; only status zero indicates success.
    pub fn parse(record: &[u8]) -> Result<Self, BootstrapError> {
        check_record_version::<DAEMON_READY_LEN>(record)?;
        let status = u32::from_le_bytes(record[4..8].try_into().expect("fixed width status"));
        Ok(Self::new(status))
    }
}

/// Failure to parse or validate a bootstrap value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BootstrapError {
    TruncatedRecord { expected: usize, actual: usize },
    TrailingRecordBytes { expected: usize, actual: usize },
    UnsupportedVersion { actual: u32 },
    InvalidIdentifier,
    InvalidIdentifierHex,
    InvalidLogonSid,
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedRecord { expected, actual } => write!(
                formatter,
                "bootstrap record is truncated: expected {expected} bytes, got {actual}"
            ),
            Self::TrailingRecordBytes { expected, actual } => write!(
                formatter,
                "bootstrap record has trailing bytes: expected {expected}, got {actual}"
            ),
            Self::UnsupportedVersion { actual } => {
                write!(
                    formatter,
                    "unsupported Windows bootstrap transport version {actual}"
                )
            }
            Self::InvalidIdentifier => formatter.write_str("invalid return-pipe identifier"),
            Self::InvalidIdentifierHex => formatter
                .write_str("return-pipe identifier must be 32 lowercase hexadecimal characters"),
            Self::InvalidLogonSid => {
                formatter.write_str("invalid Windows logon SID; expected S-1-5-5-<u32>-<u32>")
            }
        }
    }
}

impl Error for BootstrapError {}

fn check_record_version<const N: usize>(record: &[u8]) -> Result<(), BootstrapError> {
    if record.len() < N {
        return Err(BootstrapError::TruncatedRecord {
            expected: N,
            actual: record.len(),
        });
    }
    if record.len() > N {
        return Err(BootstrapError::TrailingRecordBytes {
            expected: N,
            actual: record.len(),
        });
    }
    let version = u32::from_le_bytes(record[..4].try_into().expect("fixed width version"));
    if version != TRANSPORT_VERSION {
        return Err(BootstrapError::UnsupportedVersion { actual: version });
    }
    Ok(())
}

fn is_decimal_u32(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u32>().is_ok()
}

fn lowercase_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn append_lowercase_hex(output: &mut String, bytes: &[u8]) {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(HEX_DIGITS[usize::from(*byte >> 4)] as char);
        output.push(HEX_DIGITS[usize::from(*byte & 0x0f)] as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_use_little_endian_version_and_fixed_payloads() {
        let mut id_bytes = [0; NONCE_LEN];
        id_bytes[15] = 0xab;
        let id = ReturnPipeId::from_bytes(id_bytes).unwrap();
        let announce = ClientAnnounce::new(id).encode();
        assert_eq!(&announce[..4], &[1, 0, 0, 0]);
        assert_eq!(&announce[4..], &id_bytes);
        assert_eq!(ClientAnnounce::parse(&announce).unwrap().identifier(), &id);

        let challenge = [0x5a; NONCE_LEN];
        assert_eq!(
            DaemonHello::parse(&DaemonHello::new(challenge).encode())
                .unwrap()
                .challenge(),
            &challenge
        );
        assert_eq!(
            ClientEcho::parse(&ClientEcho::new(challenge).encode())
                .unwrap()
                .challenge(),
            &challenge
        );
        let ready = DaemonReady::new(READY_STATUS_OK);
        assert_eq!(ready.encode(), [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(DaemonReady::parse(&ready.encode()).unwrap(), ready);
    }

    #[test]
    fn parsers_reject_unsupported_versions_and_non_exact_widths() {
        let mut announce =
            ClientAnnounce::new(ReturnPipeId::from_bytes([1; NONCE_LEN]).unwrap()).encode();
        announce[..4].copy_from_slice(&2_u32.to_le_bytes());
        assert_eq!(
            ClientAnnounce::parse(&announce),
            Err(BootstrapError::UnsupportedVersion { actual: 2 })
        );
        assert_eq!(
            DaemonReady::parse(&[1, 0, 0]),
            Err(BootstrapError::TruncatedRecord {
                expected: DAEMON_READY_LEN,
                actual: 3
            })
        );
        assert_eq!(
            DaemonReady::parse(&[1, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(BootstrapError::TrailingRecordBytes {
                expected: DAEMON_READY_LEN,
                actual: 9
            })
        );
    }

    #[test]
    fn identifier_and_logon_sid_validation_constrain_name_scope() {
        assert_eq!(
            ReturnPipeId::from_bytes([0; NONCE_LEN]),
            Err(BootstrapError::InvalidIdentifier)
        );
        assert_eq!(
            ReturnPipeId::from_hex(&"00".repeat(NONCE_LEN)),
            Err(BootstrapError::InvalidIdentifier)
        );
        let mut zero_announce = [0; CLIENT_ANNOUNCE_LEN];
        zero_announce[..4].copy_from_slice(&TRANSPORT_VERSION.to_le_bytes());
        assert_eq!(
            ClientAnnounce::parse(&zero_announce),
            Err(BootstrapError::InvalidIdentifier)
        );
        assert_eq!(
            ReturnPipeId::from_hex(&"AB".repeat(NONCE_LEN)),
            Err(BootstrapError::InvalidIdentifierHex)
        );
        assert_eq!(
            ReturnPipeId::from_hex("0"),
            Err(BootstrapError::InvalidIdentifierHex)
        );

        let sid = LogonSid::new("S-1-5-5-12-34").unwrap();
        let id = ReturnPipeId::from_hex("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(id.to_hex(), "0123456789abcdef0123456789abcdef");
        assert_eq!(
            derive_return_pipe_name(&sid, &id),
            r"\\.\pipe\WiiLand.S-1-5-5-12-34.return.0123456789abcdef0123456789abcdef"
        );
        for invalid_sid in [
            "S-1-5-5-12",
            "S-1-5-5-12-34-extra",
            "S-1-5-5-../x-34",
            "S-1-5-5-+12-34",
        ] {
            assert_eq!(
                LogonSid::new(invalid_sid),
                Err(BootstrapError::InvalidLogonSid)
            );
        }
    }
}
