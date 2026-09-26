use std::io;

pub const MAX_SLOTS: usize = 32;
pub const MAX_REPORT_PAYLOAD: usize = 88;
#[cfg(windows)]
pub(crate) const PIPE_NAME: &str = r"\\.\pipe\WiiLandOutput.v3";
#[cfg(windows)]
pub(crate) const PROTOCOL_VERSION: u32 = 3;
#[cfg(windows)]
pub(crate) const REPORT_LAYOUT_VERSION: u32 = 2;
#[cfg(windows)]
pub(crate) const REQUEST_SIZE: usize = 112;
#[cfg(windows)]
pub(crate) const RESPONSE_SIZE: usize = 28;

#[cfg(windows)]
const OP_CREATE: u32 = 1;
#[cfg(windows)]
const OP_REPORT: u32 = 2;
#[cfg(windows)]
const OP_DESTROY: u32 = 3;
#[cfg(windows)]
const OP_HEARTBEAT: u32 = 4;

/// The two report bodies supported by the fixed WiiLand v2 HID descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ReportId {
    Gamepad = 1,
    SupplementalAxes = 2,
}

impl ReportId {
    pub const fn payload_len(self) -> usize {
        match self {
            Self::Gamepad => 17,
            Self::SupplementalAxes => 88,
        }
    }
}

impl TryFrom<u8> for ReportId {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Gamepad),
            2 => Ok(Self::SupplementalAxes),
            _ => Err(invalid_input("unknown WiiLand HID report ID")),
        }
    }
}

/// A lease issued by the broker for a driver slot and generation.
///
/// Its fields are private so callers cannot forge a lease for another pipe
/// client or reuse a stale driver generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLease {
    pub(crate) slot: u32,
    pub(crate) generation: u32,
}

impl OutputLease {
    pub const fn slot(self) -> u32 {
        self.slot
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }
}

/// One exact-sized report body. The leading HID report-ID byte is supplied
/// separately by the driver ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputReport {
    pub(crate) report_id: ReportId,
    pub(crate) payload_len: u8,
    pub(crate) payload: [u8; MAX_REPORT_PAYLOAD],
}

impl OutputReport {
    pub fn new(report_id: ReportId, payload: &[u8]) -> io::Result<Self> {
        if payload.len() != report_id.payload_len() {
            return Err(invalid_input("report body length does not match report ID"));
        }
        let mut body = [0; MAX_REPORT_PAYLOAD];
        body[..payload.len()].copy_from_slice(payload);
        Ok(Self {
            report_id,
            payload_len: payload.len() as u8,
            payload: body,
        })
    }

    pub const fn report_id(self) -> ReportId {
        self.report_id
    }

    pub const fn payload_len(self) -> usize {
        self.payload_len as usize
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload[..self.payload_len as usize]
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Create,
    Report,
    Destroy,
    Heartbeat,
}

#[cfg(windows)]
impl Operation {
    const fn raw(self) -> u32 {
        match self {
            Self::Create => OP_CREATE,
            Self::Report => OP_REPORT,
            Self::Destroy => OP_DESTROY,
            Self::Heartbeat => OP_HEARTBEAT,
        }
    }

    fn from_raw(raw: u32) -> Result<Self, u32> {
        match raw {
            OP_CREATE => Ok(Self::Create),
            OP_REPORT => Ok(Self::Report),
            OP_DESTROY => Ok(Self::Destroy),
            OP_HEARTBEAT => Ok(Self::Heartbeat),
            _ => Err(winerr::ERROR_INVALID_DATA),
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Request {
    pub operation: Operation,
    pub slot: u32,
    pub generation: u32,
    pub report_id: u8,
    pub payload_len: u8,
    pub payload: [u8; MAX_REPORT_PAYLOAD],
}

#[cfg(windows)]
impl Request {
    pub(crate) fn create() -> Self {
        Self {
            operation: Operation::Create,
            slot: 0,
            generation: 0,
            report_id: 0,
            payload_len: 0,
            payload: [0; MAX_REPORT_PAYLOAD],
        }
    }

    pub(crate) fn heartbeat() -> Self {
        Self {
            operation: Operation::Heartbeat,
            slot: 0,
            generation: 0,
            report_id: 0,
            payload_len: 0,
            payload: [0; MAX_REPORT_PAYLOAD],
        }
    }

    pub(crate) fn report(lease: OutputLease, report: &OutputReport) -> Self {
        Self {
            operation: Operation::Report,
            slot: lease.slot,
            generation: lease.generation,
            report_id: report.report_id as u8,
            payload_len: report.payload_len,
            payload: report.payload,
        }
    }

    pub(crate) fn destroy(lease: OutputLease) -> Self {
        Self {
            operation: Operation::Destroy,
            slot: lease.slot,
            generation: lease.generation,
            report_id: 0,
            payload_len: 0,
            payload: [0; MAX_REPORT_PAYLOAD],
        }
    }

    pub(crate) fn encode(self) -> [u8; REQUEST_SIZE] {
        let mut bytes = [0; REQUEST_SIZE];
        bytes[0..4].copy_from_slice(&(REQUEST_SIZE as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.operation.raw().to_le_bytes());
        bytes[12..16].copy_from_slice(&self.slot.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.generation.to_le_bytes());
        bytes[20] = self.report_id;
        bytes[21] = self.payload_len;
        bytes[24..].copy_from_slice(&self.payload);
        bytes
    }

    pub(crate) fn decode(bytes: &[u8; REQUEST_SIZE]) -> Result<Self, u32> {
        if read_u32(bytes, 0) != REQUEST_SIZE as u32
            || read_u32(bytes, 4) != PROTOCOL_VERSION
            || bytes[22] != 0
            || bytes[23] != 0
        {
            return Err(winerr::ERROR_INVALID_DATA);
        }

        let operation = Operation::from_raw(read_u32(bytes, 8))?;
        let slot = read_u32(bytes, 12);
        let generation = read_u32(bytes, 16);
        let report_id = bytes[20];
        let payload_len = bytes[21];
        let mut payload = [0; MAX_REPORT_PAYLOAD];
        payload.copy_from_slice(&bytes[24..]);
        let request = Self {
            operation,
            slot,
            generation,
            report_id,
            payload_len,
            payload,
        };

        let valid = match operation {
            Operation::Create | Operation::Heartbeat => {
                slot == 0
                    && generation == 0
                    && report_id == 0
                    && payload_len == 0
                    && request.payload.iter().all(|byte| *byte == 0)
            }
            Operation::Report => {
                slot < MAX_SLOTS as u32
                    && generation != 0
                    && ReportId::try_from(report_id)
                        .is_ok_and(|id| payload_len as usize == id.payload_len())
                    && request.payload[payload_len as usize..]
                        .iter()
                        .all(|byte| *byte == 0)
            }
            Operation::Destroy => {
                slot < MAX_SLOTS as u32
                    && generation != 0
                    && report_id == 0
                    && payload_len == 0
                    && request.payload.iter().all(|byte| *byte == 0)
            }
        };
        if valid {
            Ok(request)
        } else {
            Err(winerr::ERROR_INVALID_PARAMETER)
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Response {
    pub operation: Operation,
    pub status: u32,
    pub slot: u32,
    pub generation: u32,
    pub report_layout_version: u32,
}

#[cfg(windows)]
impl Response {
    pub(crate) fn encode(self) -> [u8; RESPONSE_SIZE] {
        let mut bytes = [0; RESPONSE_SIZE];
        bytes[0..4].copy_from_slice(&(RESPONSE_SIZE as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.operation.raw().to_le_bytes());
        bytes[12..16].copy_from_slice(&self.status.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.slot.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.generation.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.report_layout_version.to_le_bytes());
        bytes
    }

    pub(crate) fn decode(bytes: &[u8; RESPONSE_SIZE]) -> io::Result<Self> {
        if read_u32(bytes, 0) != RESPONSE_SIZE as u32 || read_u32(bytes, 4) != PROTOCOL_VERSION {
            return Err(invalid_data("invalid WiiLand output service response"));
        }
        let operation = Operation::from_raw(read_u32(bytes, 8))
            .map_err(|_| invalid_data("unknown operation in WiiLand output response"))?;
        Ok(Self {
            operation,
            status: read_u32(bytes, 12),
            slot: read_u32(bytes, 16),
            generation: read_u32(bytes, 20),
            report_layout_version: read_u32(bytes, 24),
        })
    }
}

#[cfg(windows)]
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed protocol field"),
    )
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(windows)]
fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(windows)]
pub(crate) mod winerr {
    pub const ERROR_INVALID_DATA: u32 = 13;
    pub const ERROR_INVALID_HANDLE: u32 = 6;
    pub const ERROR_INVALID_PARAMETER: u32 = 87;
    pub const ERROR_NOT_READY: u32 = 21;
    pub const ERROR_NOT_ENOUGH_QUOTA: u32 = 1816;
    pub const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;
    pub const ERROR_BUSY: u32 = 170;
}
