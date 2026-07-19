/*
 * Copyright © 2024 Valve Software
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

//! Hardware devices interfaces

use linux_cec_sys::constants::{
    CEC_CONNECTOR_TYPE_DRM, CEC_CONNECTOR_TYPE_NO_CONNECTOR, CEC_EVENT_LOST_MSGS,
    CEC_EVENT_PIN_5V_HIGH, CEC_EVENT_PIN_5V_LOW, CEC_EVENT_PIN_CEC_HIGH, CEC_EVENT_PIN_CEC_LOW,
    CEC_EVENT_PIN_HPD_HIGH, CEC_EVENT_PIN_HPD_LOW, CEC_EVENT_STATE_CHANGE, CEC_MAX_LOG_ADDRS,
    CEC_MAX_MSG_SIZE,
};
use linux_cec_sys::ioctls::{
    adapter_get_capabilities, adapter_get_connector_info, adapter_get_logical_addresses,
    adapter_get_physical_address, adapter_set_logical_addresses, adapter_set_physical_address,
    dequeue_event, get_mode, receive_message, set_mode, transmit_message,
};
use linux_cec_sys::structs::{
    cec_caps, cec_connector_info, cec_drm_connector_info, cec_event, cec_log_addrs, cec_msg,
    CEC_RX_STATUS, CEC_TX_STATUS,
};
use linux_cec_sys::{PhysicalAddress as SysPhysicalAddress, Timestamp, VendorId as SysVendorId};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags};
use num_enum::TryFromPrimitive;
use std::ffi::{c_char, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::str::FromStr;
use tinyvec::ArrayVec;
#[cfg(feature = "tracing")]
use tracing::{debug, trace, warn};

pub use linux_cec_sys::structs::CEC_CAP as Capabilities;
pub use nix::poll::PollTimeout;

use crate::ioctls::CecMessageHandlingMode;
use crate::message::{Message, Opcode};
use crate::operand::{BufferOperand, UiCommand};
use crate::{
    Error, FollowerMode, InitiatorMode, LogicalAddress, LogicalAddressType, PhysicalAddress, Range,
    Result, RxError, Timeout, VendorId,
};

#[cfg(feature = "async")]
pub use crate::async_support::{AsyncDevice, AsyncDevicePoller};

/// An object for interacting with system CEC devices.
#[derive(Debug)]
pub struct Device {
    file: File,
    tx_logical_address: LogicalAddress,
    internal_log_addrs: cec_log_addrs,
}

/// An enum containing the message data, either successfully parsed or invalid.
#[derive(Debug, Copy, Clone, PartialEq, Hash)]
pub enum MessageData {
    /// Valid, parsed data
    Valid(Message),
    /// Invalid, unparsed data
    Invalid(ArrayVec<[u8; 14]>),
}

impl MessageData {
    #[must_use]
    pub fn opcode(&self) -> u8 {
        match self {
            MessageData::Valid(message) => message.opcode().into(),
            MessageData::Invalid(bytes) => bytes[0],
        }
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            MessageData::Valid(message) => message.to_bytes(),
            MessageData::Invalid(bytes) => bytes.to_vec(),
        }
    }
}

/// A representation of a received [`Message`] and its associated metadata.
#[derive(Debug, Clone, Hash)]
pub struct Envelope {
    /// The received message data.
    pub message: MessageData,
    /// The logical address of the CEC device that sent the message.
    pub initiator: LogicalAddress,
    /// The logical address to which this message was sent. Unless the [`Device`]
    /// has the [`FollowerMode`] set to either [`FollowerMode::Monitor`] or
    /// [`FollowerMode::MonitorAll`], this value will either be the logical address
    /// of the `Device` itself or [`LogicalAddress::Broadcast`].
    pub destination: LogicalAddress,
    /// The time at which this message was received. This may be different
    /// from the time at which the message was read out of the kernel.
    pub timestamp: Timestamp,
    /// A system-tracked sequence number.
    pub sequence: u32,
}

impl TryFrom<cec_msg> for Envelope {
    type Error = Error;

    fn try_from(message: cec_msg) -> Result<Envelope> {
        if message.rx_status.contains(CEC_RX_STATUS::TIMEOUT) {
            return Err(Error::Timeout);
        }
        if message.rx_status.contains(CEC_RX_STATUS::ABORTED) {
            return Err(Error::Abort);
        }
        if message.rx_status.contains(CEC_RX_STATUS::FEATURE_ABORT) {
            return Err(RxError::FeatureAbort.into());
        }
        if !(2..=CEC_MAX_MSG_SIZE as u32).contains(&message.len) {
            #[cfg(feature = "tracing")]
            warn!(
                "Incoming CEC message has invalid length {} (header: {:#04x}, rx status: {:?})",
                message.len, message.msg[0], message.rx_status
            );
            return Err(Error::InvalidData);
        }
        let bytes = &message.msg[1..message.len as usize];
        let initiator = LogicalAddress::try_from_primitive(message.msg[0] >> 4)?;
        let destination = LogicalAddress::try_from_primitive(message.msg[0] & 0xF)?;
        let timestamp = message.rx_ts;
        let sequence = message.sequence;

        let message = match Message::try_from_bytes(bytes) {
            Ok(message) => MessageData::Valid(message),
            Err(e) => {
                #[cfg(feature = "tracing")]
                warn!("Failed to parse incoming message {bytes:?}: {e}");
                let _ = e;
                let invalid_len = usize::min(bytes.len(), 14);
                MessageData::Invalid(ArrayVec::from_array_len(
                    message.msg[1..15].try_into().unwrap(),
                    invalid_len,
                ))
            }
        };

        let envelope = Envelope {
            message,
            initiator,
            destination,
            timestamp,
            sequence,
        };
        #[cfg(feature = "tracing")]
        trace!("Got message {envelope:?}");
        Ok(envelope)
    }
}

#[cfg(test)]
mod test_envelope {
    use super::*;
    use crate::sys::CEC_MSG_FL;

    #[test]
    fn decode_simple() {
        let msg = cec_msg {
            tx_ts: 0,
            rx_ts: 911462400,
            len: 2,
            timeout: 0,
            sequence: 1,
            flags: CEC_MSG_FL::empty(),
            msg: [
                0xF,
                Opcode::Standby as u8,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ],
            reply: 0,
            rx_status: CEC_RX_STATUS::OK,
            tx_status: CEC_TX_STATUS::empty(),
            tx_arb_lost_cnt: 0,
            tx_nack_cnt: 0,
            tx_low_drive_cnt: 0,
            tx_error_cnt: 0,
        };

        let envelope = Envelope::try_from(msg).unwrap();
        assert_eq!(envelope.message.opcode(), Opcode::Standby as u8);
        assert_eq!(envelope.initiator, LogicalAddress::Tv);
        assert_eq!(envelope.destination, LogicalAddress::Broadcast);
        assert_eq!(envelope.timestamp, 911462400);
        assert_eq!(envelope.sequence, 1);
        let MessageData::Valid(message) = envelope.message else {
            panic!();
        };
        assert_eq!(message.opcode(), Opcode::Standby);
    }

    #[test]
    fn decode_invalid_opcode() {
        let msg = cec_msg {
            tx_ts: 0,
            rx_ts: 0,
            len: 2,
            timeout: 0,
            sequence: 1,
            flags: CEC_MSG_FL::empty(),
            msg: [0xF, 0xFE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            reply: 0,
            rx_status: CEC_RX_STATUS::OK,
            tx_status: CEC_TX_STATUS::empty(),
            tx_arb_lost_cnt: 0,
            tx_nack_cnt: 0,
            tx_low_drive_cnt: 0,
            tx_error_cnt: 0,
        };

        let envelope = Envelope::try_from(msg).unwrap();
        assert_eq!(envelope.message.opcode(), 0xFE);
        let MessageData::Invalid(message) = envelope.message else {
            panic!();
        };
        assert_eq!(message.as_slice(), &[0xFE]);
    }

    #[test]
    fn decode_max_length() {
        let msg = cec_msg {
            tx_ts: 0,
            rx_ts: 0,
            len: 16,
            timeout: 0,
            sequence: 1,
            flags: CEC_MSG_FL::empty(),
            msg: [
                0xF,
                Opcode::VendorCommandWithId as u8,
                0x08,
                0x00,
                0x46,
                0x00,
                0x13,
                0x00,
                0x10,
                0x80,
                0x90,
                0x01,
                0x00,
                0x00,
                0x00,
                0x00,
            ],
            reply: 0,
            rx_status: CEC_RX_STATUS::OK,
            tx_status: CEC_TX_STATUS::empty(),
            tx_arb_lost_cnt: 0,
            tx_nack_cnt: 0,
            tx_low_drive_cnt: 0,
            tx_error_cnt: 0,
        };

        let envelope = Envelope::try_from(msg).unwrap();
        assert_eq!(envelope.message.opcode(), Opcode::VendorCommandWithId as u8);
        assert!(matches!(envelope.message, MessageData::Valid(_)));
    }

    #[test]
    fn decode_invalid_max_length() {
        let mut msg = cec_msg::from_timeout(0);
        msg.len = CEC_MAX_MSG_SIZE as u32;
        msg.msg[0] = 0x0F;
        msg.msg[1] = 0xFE;
        msg.rx_status = CEC_RX_STATUS::OK;

        let envelope = Envelope::try_from(msg).unwrap();
        let MessageData::Invalid(message) = envelope.message else {
            panic!();
        };
        assert_eq!(message.len(), 14);
        assert_eq!(message[0], 0xFE);
    }

    #[test]
    fn decode_too_long() {
        let mut msg = cec_msg::from_timeout(0);
        msg.len = CEC_MAX_MSG_SIZE as u32 + 1;
        msg.rx_status = CEC_RX_STATUS::OK;

        let Err(err) = Envelope::try_from(msg) else {
            panic!();
        };
        assert_eq!(err, Error::InvalidData);
    }

    #[test]
    fn decode_way_too_short() {
        let msg = cec_msg {
            tx_ts: 0,
            rx_ts: 0,
            len: 0,
            timeout: 0,
            sequence: 1,
            flags: CEC_MSG_FL::empty(),
            msg: [0; 16],
            reply: 0,
            rx_status: CEC_RX_STATUS::OK,
            tx_status: CEC_TX_STATUS::empty(),
            tx_arb_lost_cnt: 0,
            tx_nack_cnt: 0,
            tx_low_drive_cnt: 0,
            tx_error_cnt: 0,
        };

        let Err(err) = Envelope::try_from(msg) else {
            panic!();
        };
        assert_eq!(err, Error::InvalidData);
    }

    #[test]
    fn decode_too_short() {
        let msg = cec_msg {
            tx_ts: 0,
            rx_ts: 0,
            len: 1,
            timeout: 0,
            sequence: 1,
            flags: CEC_MSG_FL::empty(),
            msg: [0; 16],
            reply: 0,
            rx_status: CEC_RX_STATUS::OK,
            tx_status: CEC_TX_STATUS::empty(),
            tx_arb_lost_cnt: 0,
            tx_nack_cnt: 0,
            tx_low_drive_cnt: 0,
            tx_error_cnt: 0,
        };

        let Err(err) = Envelope::try_from(msg) else {
            panic!();
        };
        assert_eq!(err, Error::InvalidData);
    }
}

/// A physical pin that can be monitored via [`PinEvent`]s.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Pin {
    /// The CEC data pin (13)
    Cec,
    /// The hot plug detect (HPD) pin (19)
    HotPlugDetect,
    /// The +5 V power pin (18)
    Power5V,
}

/// The logic level of a physical pin as reported in a [`PinEvent`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum PinState {
    Low,
    High,
}

/// An event representing a logic level change of a physical pin.
///
/// To receive `PinEvent`s from a [`Device`], it must be configured with a
/// [`FollowerMode`] of either [`FollowerMode::MonitorPin`] or
/// [`FollowerMode::MonitorAll`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct PinEvent {
    /// Which physical pin generated the event.
    pub pin: Pin,
    /// The logic level of the pin.
    pub state: PinState,
}

/// An object used for polling the status of the event and message queues in the
/// kernel without borrowing the [`Device`], created by [`Device::get_poller`].
#[derive(Debug)]
pub struct DevicePoller {
    fd: OwnedFd,
}

/// Information from a [`DevicePoller`] about which information is available
/// from the kernel, to be passed to [`Device::handle_status`].
///
/// As this is a representation of what data is available and not used for
/// requesting data manually, it should not be constructed directly.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PollStatus {
    Nothing,
    Destroyed,
    GotEvent,
    GotMessage,
    GotAll,
}

/// A return result of [`Device::handle_status`], containing
/// about a single message or event was returned from the kernel.
#[derive(Debug, Clone, Hash)]
#[non_exhaustive]
pub enum PollResult {
    /// The device received a [`Message`].
    Message(Envelope),
    /// A monitored pin changed state.
    PinEvent(PinEvent),
    /// The message queue was full and a number of messages were
    /// received and lost. To avoid this, make sure to poll as
    /// frequently as possible.
    LostMessages(u32),
    /// The device state changed. Usually this means the device was configured, unconfigured,
    /// or the physical address changed. If the caller has cached any properties like logical
    /// address or physical address, these values should be refreshed.
    StateChange,
}

/// Information about how the CEC device is connected to the system.
#[derive(Debug, Clone, PartialEq, Hash)]
#[non_exhaustive]
pub enum ConnectorInfo {
    None,
    /// Tells which drm connector is associated with the CEC adapter.
    DrmConnector {
        /// drm card number
        card_no: u32,
        /// drm connector ID
        connector_id: u32,
    },
    Unknown {
        ty: u32,
        data: [u32; 16],
    },
}

impl Device {
    /// Open a CEC device from a given `path`. Generally, this will be of the
    /// form `/dev/cecX`.
    pub fn open(path: impl AsRef<Path>) -> Result<Device> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .open(path)?;
        Device::try_from(file)
    }

    /// Get a new [`DevicePoller`] object that can be used to poll the status
    /// of the kernel queue without having to borrow the `Device` object itself.
    ///
    /// While the poller can return a [`PollStatus`] to say which kinds of
    /// events or messages are available, it must be passed to
    /// [`handle_status`](Device::handle_status), which will require borrowing
    /// the `Device`, to get the actual data.
    pub fn get_poller(&self) -> Result<DevicePoller> {
        Ok(DevicePoller {
            fd: self.file.as_fd().try_clone_to_owned()?,
        })
    }

    /// Wait up to `timeout` for a message or event and process the results.
    /// This is a convenience method that combines using a `DevicePoller`
    /// and calling `handle_status` internally.
    pub fn poll(&mut self, timeout: PollTimeout) -> Result<Vec<PollResult>> {
        let poller = self.get_poller()?;
        let status = poller.poll(timeout)?;
        self.handle_status(status)
    }

    /// Set or clear `O_NONBLOCK` on the underlying fd.
    pub fn set_blocking(&self, blocking: bool) -> Result<()> {
        let rawfd = self.file.as_fd();
        let mut flags = OFlag::from_bits_retain(fcntl(rawfd, FcntlArg::F_GETFL)?);
        flags.set(OFlag::O_NONBLOCK, !blocking);
        fcntl(rawfd, FcntlArg::F_SETFL(flags))?;
        Ok(())
    }

    /// Get the [`InitiatorMode`] of the device.
    pub fn get_initiator_mode(&self) -> Result<InitiatorMode> {
        self.get_mode()?.initiator().try_into()
    }

    /// Set the [`InitiatorMode`] of the device.
    pub fn set_initiator_mode(&self, mode: InitiatorMode) -> Result<()> {
        let mode = self.get_mode()?.with_initiator(mode.into());
        self.set_mode(mode)
    }

    /// Get the [`FollowerMode`] of the device.
    pub fn get_follower_mode(&self) -> Result<FollowerMode> {
        self.get_mode()?.follower().try_into()
    }

    /// Set the [`FollowerMode`] of the device.
    pub fn set_follower_mode(&self, mode: FollowerMode) -> Result<()> {
        let mode = self.get_mode()?.with_follower(mode.into());
        self.set_mode(mode)
    }

    /// Get the raw [`cec_caps`] struct for the device from the kernel.
    pub fn get_raw_capabilities(&self) -> Result<cec_caps> {
        let mut caps = cec_caps::default();
        unsafe {
            adapter_get_capabilities(self.file.as_raw_fd(), &mut caps)?;
        }
        Ok(caps)
    }

    /// Get the [`Capabilities`] of the device, informing the quirks of
    /// the specific device.
    pub fn get_capabilities(&self) -> Result<Capabilities> {
        self.get_raw_capabilities().map(|caps| caps.capabilities)
    }

    /// Get the name of the driver that is backing this device.
    pub fn get_driver_name(&self) -> Result<OsString> {
        self.get_raw_capabilities().map(|caps| {
            let driver =
                unsafe { &*std::ptr::from_ref::<[c_char; 32]>(&caps.driver).cast::<[u8; 32]>() };
            OsStr::from_bytes(driver).to_os_string()
        })
    }

    /// Get the name of the adapter that is backing this device.
    pub fn get_adapter_name(&self) -> Result<OsString> {
        self.get_raw_capabilities().map(|caps| {
            let adapter =
                unsafe { &*std::ptr::from_ref::<[c_char; 32]>(&caps.name).cast::<[u8; 32]>() };
            OsStr::from_bytes(adapter).to_os_string()
        })
    }

    /// Get the currently configured [`PhysicalAddress`] of the device.
    pub fn get_physical_address(&self) -> Result<PhysicalAddress> {
        let mut phys_addr: SysPhysicalAddress = 0;
        unsafe {
            adapter_get_physical_address(self.file.as_raw_fd(), &mut phys_addr)?;
        }
        Ok(PhysicalAddress(phys_addr))
    }

    /// Set the [`PhysicalAddress`] of the device. This function will only work if the capability
    /// [`Capabilities::PHYS_ADDR`] is present.
    pub fn set_physical_address(&self, phys_addr: PhysicalAddress) -> Result<()> {
        unsafe {
            adapter_set_physical_address(self.file.as_raw_fd(), &phys_addr.0)?;
        }
        Ok(())
    }

    /// Get the currently configured [`LogicalAddress`]es of the device.
    pub fn get_logical_addresses(&mut self) -> Result<Vec<LogicalAddress>> {
        unsafe {
            adapter_get_logical_addresses(self.file.as_raw_fd(), &mut self.internal_log_addrs)?;
        }

        let mut vec = Vec::new();
        for index in 0..self.internal_log_addrs.num_log_addrs {
            vec.push(self.internal_log_addrs.log_addr[index as usize].try_into()?);
        }
        Ok(vec)
    }

    /// Set the [`LogicalAddress`]es of the device. This function will only work if the capability
    /// [`Capabilities::LOG_ADDRS`] is present. Note that the device must be configured to act as
    /// an initiator via [`Device::set_initiator_mode`], otherwise this function will return an error.
    pub fn set_logical_addresses(&mut self, log_addrs: &[LogicalAddressType]) -> Result<()> {
        Range::AtMost(CEC_MAX_LOG_ADDRS).check(log_addrs.len(), "logical addresses")?;

        #[cfg(feature = "tracing")]
        debug!("Attempting to set logical addresses: {log_addrs:?}");

        for (index, log_addr) in log_addrs.iter().enumerate() {
            self.internal_log_addrs.log_addr_type[index] = (*log_addr).into();
            if let Some(prim_dev_type) = (*log_addr).primary_device_type() {
                self.internal_log_addrs.primary_device_type[index] = prim_dev_type.into();
            } else {
                self.internal_log_addrs.primary_device_type[index] = 0xFF;
            }
        }

        if !log_addrs.is_empty() && self.internal_log_addrs.num_log_addrs > 0 {
            // Clear old logical addresses first, if present
            self.clear_logical_addresses()?;
        }

        self.internal_log_addrs.num_log_addrs = log_addrs.len().try_into().unwrap();
        unsafe {
            adapter_set_logical_addresses(self.file.as_raw_fd(), &mut self.internal_log_addrs)?;
        }
        if !log_addrs.is_empty() {
            self.tx_logical_address =
                LogicalAddress::try_from_primitive(self.internal_log_addrs.log_addr[0])
                    .unwrap_or(LogicalAddress::Unregistered);
        }
        Ok(())
    }

    /// Set the single [`LogicalAddress`] of the device. This function will only work if the capability
    /// [`Capabilities::LOG_ADDRS`] is present. Note that the device must be configured to act as
    /// an initiator via [`Device::set_initiator_mode`], otherwise this function will return an error.
    pub fn set_logical_address(&mut self, log_addr: LogicalAddressType) -> Result<()> {
        self.set_logical_addresses(&[log_addr])
    }

    /// Clear the [`LogicalAddress`]es of the device. This function will only work if the capability
    /// [`Capabilities::LOG_ADDRS`] is present.
    pub fn clear_logical_addresses(&mut self) -> Result<()> {
        self.internal_log_addrs.num_log_addrs = 0;
        self.tx_logical_address = LogicalAddress::Unregistered;
        unsafe {
            adapter_set_logical_addresses(self.file.as_raw_fd(), &mut self.internal_log_addrs)?;
        }
        Ok(())
    }

    /// Get the configured OSD name.
    pub fn get_osd_name(&mut self) -> Result<OsString> {
        unsafe {
            adapter_get_logical_addresses(self.file.as_raw_fd(), &mut self.internal_log_addrs)?;
        }
        Ok(OsStr::from_bytes(&self.internal_log_addrs.osd_name).to_os_string())
    }

    /// Set the advertised OSD name. This is used by TV sets to display the name
    /// of connected devices. The maximum length allowed is only 14 bytes, and is
    /// defined by the specification to be only ASCII. However, some TV sets will
    /// properly display UTF-8 text as well. Note that the encoding *does not*
    /// affect the allowable length of bytes, so using any multi-byte characters
    /// will effectively decrease the maximum number of characters.
    ///
    /// For the kernel to properly advertise the OSD name on query, you must call
    /// this function *before* calling [`Device::set_logical_addresses`].
    ///
    /// # Errors
    /// This function will raise an [`Error::OutOfRange`] error if the name passed
    /// exceeds 14 bytes.
    pub fn set_osd_name(&mut self, name: &str) -> Result<()> {
        let name_buffer = BufferOperand::from_str(name)?;
        #[cfg(feature = "tracing")]
        debug!("Setting OSD name to {name}");
        self.internal_log_addrs.osd_name[..14].copy_from_slice(&name_buffer.buffer);
        if self.tx_logical_address != LogicalAddress::Unregistered {
            let message = Message::SetOsdName { name: name_buffer };
            self.tx_message(&message, LogicalAddress::Tv)?;
        }
        Ok(())
    }

    /// Get the [`VendorId`] of this device, if configured.
    pub fn get_vendor_id(&mut self) -> Result<Option<VendorId>> {
        unsafe {
            adapter_get_logical_addresses(self.file.as_raw_fd(), &mut self.internal_log_addrs)?;
        }
        VendorId::try_from_sys(self.internal_log_addrs.vendor_id)
    }

    /// Set the advertised [`VendorId`] of the device. You should generally
    /// use the OUI assigned to your organization, if applicable. You must call
    /// this function *before* calling [`Device::set_logical_addresses`].
    pub fn set_vendor_id(&mut self, vendor_id: Option<VendorId>) -> Result<()> {
        if let Some(vendor_id) = vendor_id {
            self.internal_log_addrs.vendor_id = vendor_id.into();
            #[cfg(feature = "tracing")]
            debug!(
                "Setting vendor ID to {:02X}-{:02X}-{:02X}",
                vendor_id[0], vendor_id[1], vendor_id[2]
            );
        } else {
            #[cfg(feature = "tracing")]
            debug!("Clearing vendor ID");
            self.internal_log_addrs.vendor_id = SysVendorId::default();
        }
        Ok(())
    }

    /// Transmit a [`Message`] to a given [`LogicalAddress`]. Use
    /// [`LogicalAddress::Broadcast`] for broadcasting to all attached devices.
    /// The sequence number of the submitted message is returned.
    pub fn tx_message(&self, message: &Message, destination: LogicalAddress) -> Result<u32> {
        let reply =
            self.tx_rx_message(message, destination, Opcode::FeatureAbort, Timeout::NONE)?;
        Ok(reply.sequence)
    }

    /// Transmit a raw system [`cec_msg`] directly through the `CEC_TRANSMIT` ioctl.
    pub fn tx_raw_message(&self, message: &mut cec_msg) -> Result<()> {
        unsafe {
            transmit_message(self.file.as_raw_fd(), message)?;
        }
        Ok(())
    }

    /// Transmit a [`Message`] to a given [`LogicalAddress`] and wait for a reply of
    /// a given [`Opcode`]. Use [`LogicalAddress::Broadcast`] for broadcasting to all
    /// attached devices. Note that the timeout cannot be 0 or more than 1 second,
    /// otherwise they will be coerced to 1 second.
    pub fn tx_rx_message(
        &self,
        message: &Message,
        destination: LogicalAddress,
        reply: Opcode,
        timeout: Timeout,
    ) -> Result<Envelope> {
        let mut raw_message = cec_msg::new(self.tx_logical_address.into(), destination.into());
        let bytes = message.to_bytes();
        let len = usize::min(bytes.len(), 15) + 1;
        raw_message.len = len.try_into().unwrap();
        raw_message.msg[1..len].copy_from_slice(&bytes[..len - 1]);
        raw_message.reply = reply.into();
        raw_message.timeout = timeout.as_ms();
        #[cfg(feature = "tracing")]
        trace!(
            "Sending message {message:?} to {destination} ({:x})",
            destination as u8
        );
        self.tx_raw_message(&mut raw_message)?;
        if !raw_message.tx_status.contains(CEC_TX_STATUS::OK) {
            #[cfg(feature = "tracing")]
            warn!(
                "Message {message:?} to {destination} ({:x}) failed to send: {:?}",
                destination as u8, raw_message.tx_status
            );
            return Err(raw_message.tx_status.into());
        }
        raw_message.try_into()
    }

    /// Receive a message, waiting up to a [`Timeout`] if one is not available
    /// in the kernel buffer already. The resulting [`Envelope`] will contain the
    /// message and associated metadata.
    pub fn rx_message(&self, timeout: Timeout) -> Result<Envelope> {
        self.rx_raw_message(timeout.as_ms())?.try_into()
    }

    /// Receive a raw system [`cec_msg`] directly through the `CEC_RECEIVE` ioctl.
    pub fn rx_raw_message(&self, timeout_ms: u32) -> Result<cec_msg> {
        let mut message = cec_msg::from_timeout(timeout_ms);
        unsafe {
            receive_message(self.file.as_raw_fd(), &mut message)?;
        }
        Ok(message)
    }

    /// Poll the bus for the presence of a given [`LogicalAddress`]. If the
    /// device is present, this returns `Ok`, otherwise it returns with a
    /// specific error detailing the manner of failure. Usually this will be
    /// [`TxError::Nack`](crate::TxError::Nack) if the device isn't present,
    /// but other manners of failure are theoretically possible.
    pub fn poll_address(&self, destination: LogicalAddress) -> Result<()> {
        let mut raw_message = cec_msg::new(self.tx_logical_address.into(), destination.into());
        #[cfg(feature = "tracing")]
        trace!("Sending poll to {destination} ({:x})", destination as u8);
        self.tx_raw_message(&mut raw_message)?;
        if !raw_message.tx_status.contains(CEC_TX_STATUS::OK) {
            #[cfg(feature = "tracing")]
            warn!(
                "Poll to {destination} ({:x}) failed: {:?}",
                destination as u8, raw_message.tx_status
            );
            return Err(raw_message.tx_status.into());
        }
        Ok(())
    }

    pub(crate) fn dequeue_event(&self) -> Result<cec_event> {
        let mut event = cec_event::default();
        unsafe {
            dequeue_event(self.file.as_raw_fd(), &mut event)?;
        }
        Ok(event)
    }

    pub(crate) fn get_mode(&self) -> Result<CecMessageHandlingMode> {
        let mut mode = 0u32;
        unsafe {
            get_mode(self.file.as_raw_fd(), &mut mode)?;
        }
        Ok(mode.into())
    }

    pub(crate) fn set_mode(&self, mode: CecMessageHandlingMode) -> Result<()> {
        unsafe {
            set_mode(self.file.as_raw_fd(), &mode.into())?;
        }
        Ok(())
    }

    /// Handle the [`PollStatus`] returned from [`DevicePoller::poll`]. This
    /// will dequeue any indicated messages or events, handle any internal
    /// processing, and return information about the events or [`Envelope`]s
    /// containing [`Message`]s.
    pub fn handle_status(&mut self, status: PollStatus) -> Result<Vec<PollResult>> {
        let mut results = Vec::new();
        if status.got_event() {
            let ev = self.dequeue_event()?;
            match ev.event {
                CEC_EVENT_STATE_CHANGE => {
                    unsafe {
                        adapter_get_logical_addresses(
                            self.file.as_raw_fd(),
                            &mut self.internal_log_addrs,
                        )?;
                    }
                    if self.internal_log_addrs.num_log_addrs > 0 {
                        self.tx_logical_address =
                            LogicalAddress::try_from_primitive(self.internal_log_addrs.log_addr[0])
                                .unwrap_or(LogicalAddress::Unregistered);
                    } else {
                        self.tx_logical_address = LogicalAddress::Unregistered;
                    }
                    results.push(PollResult::StateChange);
                }
                CEC_EVENT_LOST_MSGS => results.push(PollResult::LostMessages(unsafe {
                    ev.data.lost_msgs.lost_msgs
                })),
                CEC_EVENT_PIN_CEC_LOW => results.push(PollResult::PinEvent(PinEvent {
                    pin: Pin::Cec,
                    state: PinState::Low,
                })),
                CEC_EVENT_PIN_CEC_HIGH => results.push(PollResult::PinEvent(PinEvent {
                    pin: Pin::Cec,
                    state: PinState::High,
                })),
                CEC_EVENT_PIN_HPD_LOW => results.push(PollResult::PinEvent(PinEvent {
                    pin: Pin::HotPlugDetect,
                    state: PinState::Low,
                })),
                CEC_EVENT_PIN_HPD_HIGH => results.push(PollResult::PinEvent(PinEvent {
                    pin: Pin::HotPlugDetect,
                    state: PinState::High,
                })),
                CEC_EVENT_PIN_5V_LOW => results.push(PollResult::PinEvent(PinEvent {
                    pin: Pin::Power5V,
                    state: PinState::Low,
                })),
                CEC_EVENT_PIN_5V_HIGH => results.push(PollResult::PinEvent(PinEvent {
                    pin: Pin::Power5V,
                    state: PinState::High,
                })),
                event => {
                    #[cfg(feature = "tracing")]
                    warn!("Unknown CEC event {event} with flags {:?}", ev.flags);
                    return Err(Error::InvalidData);
                }
            }
        }

        if status.got_message() {
            results.push(PollResult::Message(self.rx_message(Timeout::from_ms(1))?));
        }

        Ok(results)
    }

    /// Get information about the connector for the device, which is usually a card handled
    /// by Linux's [DRM](https://en.wikipedia.org/wiki/Direct_Rendering_Manager) subsystem.
    pub fn get_connector_info(&self) -> Result<ConnectorInfo> {
        let mut conn_info = cec_connector_info::default();
        unsafe {
            adapter_get_connector_info(self.file.as_raw_fd(), &mut conn_info)?;
        }
        match conn_info.ty {
            CEC_CONNECTOR_TYPE_NO_CONNECTOR => Ok(ConnectorInfo::None),
            CEC_CONNECTOR_TYPE_DRM => {
                let cec_drm_connector_info {
                    card_no,
                    connector_id,
                } = unsafe { conn_info.data.drm };
                Ok(ConnectorInfo::DrmConnector {
                    card_no,
                    connector_id,
                })
            }
            ty => Ok(ConnectorInfo::Unknown {
                ty,
                data: unsafe { conn_info.data.raw },
            }),
        }
    }

    /// Tell the TV to switch to the given phyiscal address for its input. If
    /// the address is None, it uses the physical address of the device itself.
    pub fn set_active_source(&self, address: Option<PhysicalAddress>) -> Result<()> {
        let address = match address {
            Some(address) => address,
            None => self.get_physical_address()?,
        };
        let active_source = Message::ActiveSource { address };
        self.tx_message(&active_source, LogicalAddress::Broadcast)?;
        Ok(())
    }

    /// Wake the TV and optionally tell the TV to make this device the active source via
    /// the One Touch Play feature.
    ///
    /// HDMI CEC specifies two messages for how to handle device switching: Image View
    /// and Text View. The Image View is simply the video input, with the possibility of
    /// menus or infoboxes (the Text View) displayed over it. If `text_view` is set to
    /// `true`, the device will request both and the TV should dismiss any active menus.
    pub fn wake(&self, set_active: bool, text_view: bool) -> Result<()> {
        if text_view {
            let text_view_on = Message::TextViewOn {};
            self.tx_message(&text_view_on, LogicalAddress::Tv)?;
        } else {
            let image_view_on = Message::ImageViewOn {};
            self.tx_message(&image_view_on, LogicalAddress::Tv)?;
        }
        if set_active {
            let address = self.get_physical_address()?;
            let active_source = Message::ActiveSource { address };
            self.tx_message(&active_source, LogicalAddress::Broadcast)?;
        }
        Ok(())
    }

    /// Tell a device with the given [`LogicalAddress`] to enter standby mode.
    pub fn standby(&self, target: LogicalAddress) -> Result<()> {
        let standby = Message::Standby {};
        self.tx_message(&standby, target)?;
        Ok(())
    }

    /// Convenience method for sending a user control command to a given [`LogicalAddress`]
    /// by generating a [`UserControlPressed`](Message::UserControlPressed) message.
    /// These generally correspond to the buttons on a remote control that are relayed to other
    /// devices. This must be matched with a call to [`Device::release_user_control`].
    pub fn press_user_control(&self, ui_command: UiCommand, target: LogicalAddress) -> Result<()> {
        let user_control = Message::UserControlPressed { ui_command };
        self.tx_message(&user_control, target)?;
        Ok(())
    }

    /// Convenience method for terminating a user control command, as started with
    /// [`Device::press_user_control`]. Internally, this just creates and sends a
    /// [`UserControlReleased`](Message::UserControlReleased) message to the given
    /// [`LogicalAddress`].
    pub fn release_user_control(&self, target: LogicalAddress) -> Result<()> {
        let user_control = Message::UserControlReleased {};
        self.tx_message(&user_control, target)?;
        Ok(())
    }
}

impl TryFrom<File> for Device {
    type Error = Error;

    fn try_from(file: File) -> Result<Device> {
        let mut internal_log_addrs = cec_log_addrs::default();
        unsafe {
            adapter_get_logical_addresses(file.as_raw_fd(), &mut internal_log_addrs)?;
        }
        let tx_logical_address = if internal_log_addrs.num_log_addrs > 0 {
            LogicalAddress::try_from_primitive(internal_log_addrs.log_addr[0]).unwrap_or_default()
        } else {
            LogicalAddress::Unregistered
        };

        Ok(Device {
            file,
            tx_logical_address,
            internal_log_addrs,
        })
    }
}

impl DevicePoller {
    /// Poll the kernel queues for the [`Device`]. The returned [`PollStatus`]
    /// must be passed to [`Device::handle_status`] to dequeue the events or
    /// messages that the status may indicate are present.
    pub fn poll(&self, timeout: PollTimeout) -> Result<PollStatus> {
        let mut pollfd = [PollFd::new(
            self.fd.as_fd(),
            PollFlags::POLLPRI | PollFlags::POLLIN,
        )];
        let done = poll(&mut pollfd, timeout)?;

        if done == 0 {
            return Err(Error::Timeout);
        }

        match pollfd[0].revents() {
            None => Ok(PollStatus::Nothing),
            Some(flags) if flags.contains(PollFlags::POLLHUP) => Ok(PollStatus::Destroyed),
            Some(flags) if flags.contains(PollFlags::POLLIN | PollFlags::POLLPRI) => {
                Ok(PollStatus::GotAll)
            }
            Some(flags) if flags.contains(PollFlags::POLLIN) => Ok(PollStatus::GotMessage),
            Some(flags) if flags.contains(PollFlags::POLLPRI) => Ok(PollStatus::GotEvent),
            Some(_) => Err(Error::UnknownError(String::from(
                "Polling error encountered",
            ))),
        }
    }
}

impl PollStatus {
    #[must_use]
    pub fn got_message(&self) -> bool {
        matches!(self, PollStatus::GotMessage | PollStatus::GotAll)
    }

    #[must_use]
    pub fn got_event(&self) -> bool {
        matches!(self, PollStatus::GotEvent | PollStatus::GotAll)
    }
}
