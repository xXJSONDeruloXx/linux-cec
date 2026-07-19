/*
 * Copyright © 2024 Valve Software
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

//! Message encoding, decoding and handling

#![allow(clippy::len_without_is_empty)]

#[cfg(test)]
use linux_cec_macros::message_test;
use linux_cec_macros::{MessageEnum, Operand};
use num_enum::{IntoPrimitive, TryFromPrimitive};
#[cfg(test)]
use std::str::FromStr;

use crate::operand::OperandEncodable;
use crate::{cdc, constants, operand, AddressingType, PhysicalAddress, Result};
#[cfg(test)]
use crate::{Error, Range};

pub use crate::cdc::{Message as CdcMessage, Opcode as CdcOpcode};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, MessageEnum)]
#[repr(u8)]
#[non_exhaustive]
pub enum Message {
    #[addressing = "broadcast"]
    ActiveSource {
        address: PhysicalAddress,
    } = constants::CEC_MSG_ACTIVE_SOURCE,
    ImageViewOn = constants::CEC_MSG_IMAGE_VIEW_ON,
    TextViewOn = constants::CEC_MSG_TEXT_VIEW_ON,
    InactiveSource {
        address: PhysicalAddress,
    } = constants::CEC_MSG_INACTIVE_SOURCE,
    #[addressing = "broadcast"]
    RequestActiveSource = constants::CEC_MSG_REQUEST_ACTIVE_SOURCE,
    #[addressing = "broadcast"]
    RoutingChange {
        original_address: PhysicalAddress,
        new_address: PhysicalAddress,
    } = constants::CEC_MSG_ROUTING_CHANGE,
    #[addressing = "broadcast"]
    RoutingInformation {
        address: PhysicalAddress,
    } = constants::CEC_MSG_ROUTING_INFORMATION,
    #[addressing = "broadcast"]
    SetStreamPath {
        address: PhysicalAddress,
    } = constants::CEC_MSG_SET_STREAM_PATH,
    #[addressing = "either"]
    Standby = constants::CEC_MSG_STANDBY,
    RecordOff = constants::CEC_MSG_RECORD_OFF,
    RecordOn {
        source: operand::RecordSource,
    } = constants::CEC_MSG_RECORD_ON,
    RecordStatus {
        status: operand::RecordStatusInfo,
    } = constants::CEC_MSG_RECORD_STATUS,
    RecordTvScreen = constants::CEC_MSG_RECORD_TV_SCREEN,
    ClearAnalogueTimer {
        day_of_month: operand::DayOfMonth,
        month_of_year: operand::MonthOfYear,
        start_time: operand::Time,
        duration: operand::Duration,
        recording_sequence: operand::RecordingSequence,
        service_id: operand::AnalogueServiceId,
    } = constants::CEC_MSG_CLEAR_ANALOGUE_TIMER,
    ClearDigitalTimer {
        day_of_month: operand::DayOfMonth,
        month_of_year: operand::MonthOfYear,
        start_time: operand::Time,
        duration: operand::Duration,
        recording_sequence: operand::RecordingSequence,
        service_id: operand::DigitalServiceId,
    } = constants::CEC_MSG_CLEAR_DIGITAL_TIMER,
    ClearExtTimer {
        day_of_month: operand::DayOfMonth,
        month_of_year: operand::MonthOfYear,
        start_time: operand::Time,
        duration: operand::Duration,
        recording_sequence: operand::RecordingSequence,
        external_source: operand::ExternalSource,
    } = constants::CEC_MSG_CLEAR_EXT_TIMER,
    SetAnalogueTimer {
        day_of_month: operand::DayOfMonth,
        month_of_year: operand::MonthOfYear,
        start_time: operand::Time,
        duration: operand::Duration,
        recording_sequence: operand::RecordingSequence,
        service_id: operand::AnalogueServiceId,
    } = constants::CEC_MSG_SET_ANALOGUE_TIMER,
    SetDigitalTimer {
        day_of_month: operand::DayOfMonth,
        month_of_year: operand::MonthOfYear,
        start_time: operand::Time,
        duration: operand::Duration,
        recording_sequence: operand::RecordingSequence,
        service_id: operand::DigitalServiceId,
    } = constants::CEC_MSG_SET_DIGITAL_TIMER,
    SetExtTimer {
        day_of_month: operand::DayOfMonth,
        month_of_year: operand::MonthOfYear,
        start_time: operand::Time,
        duration: operand::Duration,
        recording_sequence: operand::RecordingSequence,
        external_source: operand::ExternalSource,
    } = constants::CEC_MSG_SET_EXT_TIMER,
    SetTimerProgramTitle {
        title: operand::BufferOperand,
    } = constants::CEC_MSG_SET_TIMER_PROGRAM_TITLE,
    TimerClearedStatus {
        status: operand::TimerClearedStatusData,
    } = constants::CEC_MSG_TIMER_CLEARED_STATUS,
    TimerStatus {
        status: operand::TimerStatusData,
    } = constants::CEC_MSG_TIMER_STATUS,
    CecVersion {
        version: operand::Version,
    } = constants::CEC_MSG_CEC_VERSION,
    GetCecVersion = constants::CEC_MSG_GET_CEC_VERSION,
    GivePhysicalAddr = constants::CEC_MSG_GIVE_PHYSICAL_ADDR,
    GetMenuLanguage = constants::CEC_MSG_GET_MENU_LANGUAGE,
    #[addressing = "broadcast"]
    ReportPhysicalAddr {
        physical_address: PhysicalAddress,
        device_type: operand::PrimaryDeviceType,
    } = constants::CEC_MSG_REPORT_PHYSICAL_ADDR,
    #[addressing = "broadcast"]
    SetMenuLanguage {
        language: [u8; 3],
    } = constants::CEC_MSG_SET_MENU_LANGUAGE,
    DeckControl {
        mode: operand::DeckControlMode,
    } = constants::CEC_MSG_DECK_CONTROL,
    DeckStatus {
        info: operand::DeckInfo,
    } = constants::CEC_MSG_DECK_STATUS,
    GiveDeckStatus {
        request: operand::StatusRequest,
    } = constants::CEC_MSG_GIVE_DECK_STATUS,
    Play {
        mode: operand::PlayMode,
    } = constants::CEC_MSG_PLAY,
    GiveTunerDeviceStatus {
        request: operand::StatusRequest,
    } = constants::CEC_MSG_GIVE_TUNER_DEVICE_STATUS,
    SelectAnalogueService {
        service_id: operand::AnalogueServiceId,
    } = constants::CEC_MSG_SELECT_ANALOGUE_SERVICE,
    SelectDigitalService {
        service_id: operand::DigitalServiceId,
    } = constants::CEC_MSG_SELECT_DIGITAL_SERVICE,
    TunerDeviceStatus {
        info: operand::TunerDeviceInfo,
    } = constants::CEC_MSG_TUNER_DEVICE_STATUS,
    TunerStepDecrement = constants::CEC_MSG_TUNER_STEP_DECREMENT,
    TunerStepIncrement = constants::CEC_MSG_TUNER_STEP_INCREMENT,
    #[addressing = "broadcast"]
    DeviceVendorId {
        vendor_id: crate::VendorId,
    } = constants::CEC_MSG_DEVICE_VENDOR_ID,
    GiveDeviceVendorId = constants::CEC_MSG_GIVE_DEVICE_VENDOR_ID,
    VendorCommand {
        command: operand::BufferOperand,
    } = constants::CEC_MSG_VENDOR_COMMAND,
    #[addressing = "either"]
    VendorCommandWithId {
        vendor_id: crate::VendorId,
        vendor_specific_data: operand::BoundedBufferOperand<11, u8>,
    } = constants::CEC_MSG_VENDOR_COMMAND_WITH_ID,
    #[addressing = "either"]
    VendorRemoteButtonDown {
        rc_code: operand::BufferOperand,
    } = constants::CEC_MSG_VENDOR_REMOTE_BUTTON_DOWN,
    #[addressing = "either"]
    VendorRemoteButtonUp = constants::CEC_MSG_VENDOR_REMOTE_BUTTON_UP,
    SetOsdString {
        display_control: operand::DisplayControl,
        osd_string: operand::BoundedBufferOperand<13, u8>,
    } = constants::CEC_MSG_SET_OSD_STRING,
    GiveOsdName = constants::CEC_MSG_GIVE_OSD_NAME,
    SetOsdName {
        name: operand::BufferOperand,
    } = constants::CEC_MSG_SET_OSD_NAME,
    MenuRequest {
        request_type: operand::MenuRequestType,
    } = constants::CEC_MSG_MENU_REQUEST,
    MenuStatus {
        state: operand::MenuState,
    } = constants::CEC_MSG_MENU_STATUS,
    UserControlPressed {
        ui_command: operand::UiCommand,
    } = constants::CEC_MSG_USER_CONTROL_PRESSED,
    UserControlReleased = constants::CEC_MSG_USER_CONTROL_RELEASED,
    GiveDevicePowerStatus = constants::CEC_MSG_GIVE_DEVICE_POWER_STATUS,
    #[addressing = "either"]
    ReportPowerStatus {
        status: operand::PowerStatus,
    } = constants::CEC_MSG_REPORT_POWER_STATUS,
    FeatureAbort {
        opcode: u8,
        abort_reason: operand::AbortReason,
    } = constants::CEC_MSG_FEATURE_ABORT,
    Abort = constants::CEC_MSG_ABORT,
    GiveAudioStatus = constants::CEC_MSG_GIVE_AUDIO_STATUS,
    GiveSystemAudioModeStatus = constants::CEC_MSG_GIVE_SYSTEM_AUDIO_MODE_STATUS,
    ReportAudioStatus {
        status: operand::AudioStatus,
    } = constants::CEC_MSG_REPORT_AUDIO_STATUS,
    ReportShortAudioDescriptor {
        descriptors: operand::BoundedBufferOperand<4, operand::ShortAudioDescriptor>,
    } = constants::CEC_MSG_REPORT_SHORT_AUDIO_DESCRIPTOR,
    RequestShortAudioDescriptor {
        descriptors: operand::BoundedBufferOperand<4, operand::AudioFormatIdAndCode>,
    } = constants::CEC_MSG_REQUEST_SHORT_AUDIO_DESCRIPTOR,
    #[addressing = "either"]
    SetSystemAudioMode {
        status: bool,
    } = constants::CEC_MSG_SET_SYSTEM_AUDIO_MODE,
    SystemAudioModeRequest {
        physical_address: PhysicalAddress,
    } = constants::CEC_MSG_SYSTEM_AUDIO_MODE_REQUEST,
    SystemAudioModeStatus {
        status: bool,
    } = constants::CEC_MSG_SYSTEM_AUDIO_MODE_STATUS,
    SetAudioRate {
        audio_rate: operand::AudioRate,
    } = constants::CEC_MSG_SET_AUDIO_RATE,
    /* HDMI 1.4b */
    InitiateArc = constants::CEC_MSG_INITIATE_ARC,
    ReportArcInitiated = constants::CEC_MSG_REPORT_ARC_INITIATED,
    ReportArcTerminated = constants::CEC_MSG_REPORT_ARC_TERMINATED,
    RequestArcInitiation = constants::CEC_MSG_REQUEST_ARC_INITIATION,
    RequestArcTermination = constants::CEC_MSG_REQUEST_ARC_TERMINATION,
    TerminateArc = constants::CEC_MSG_TERMINATE_ARC,
    #[addressing = "broadcast"]
    CdcMessage {
        initiator: PhysicalAddress,
        message: cdc::Message,
    } = constants::CEC_MSG_CDC_MESSAGE,
    /* HDMI 2.0 */
    #[addressing = "broadcast"]
    ReportFeatures {
        version: operand::Version,
        device_types: operand::AllDeviceTypes,
        rc_profile: operand::RcProfile,
        device_features: operand::DeviceFeatures,
    } = constants::CEC_MSG_REPORT_FEATURES,
    GiveFeatures = constants::CEC_MSG_GIVE_FEATURES,
    #[addressing = "broadcast"]
    RequestCurrentLatency {
        physical_address: PhysicalAddress,
    } = constants::CEC_MSG_REQUEST_CURRENT_LATENCY,
    #[addressing = "broadcast"]
    ReportCurrentLatency {
        physical_address: PhysicalAddress,
        video_latency: operand::Delay,
        flags: operand::LatencyFlags,
        audio_output_delay: Option<operand::Delay>,
    } = constants::CEC_MSG_REPORT_CURRENT_LATENCY,
    /* HDMI 2.1a */
    SetAudioVolumeLevel {
        volume_level: operand::AudioVolumeLevel,
    } = constants::CEC_MSG_SET_AUDIO_VOLUME_LEVEL,
}

impl Message {
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        let opcode = unsafe { *<*const _>::from(self).cast::<u8>() };
        Opcode::try_from_primitive(opcode).unwrap()
    }

    /// Report whether or not this `Message` can be directly
    /// addressed to a specific logical address.
    #[must_use]
    pub fn can_directly_address(&self) -> bool {
        self.opcode().can_directly_address()
    }

    /// Report whether or not this `Message` can be
    /// broadcast to all logical addresses.
    #[must_use]
    pub fn can_broadcast(&self) -> bool {
        self.opcode().can_broadcast()
    }

    /// Get the [`AddressingType`] that reports whether this `Message` can be
    /// addressed directly to a specific logical address, broadcast to all
    /// logical addresses, or both.
    #[must_use]
    pub fn addressing_type(&self) -> AddressingType {
        self.opcode().addressing_type()
    }
}

impl Opcode {
    #[must_use]
    pub fn can_directly_address(&self) -> bool {
        matches!(
            self.addressing_type(),
            AddressingType::Direct | AddressingType::Either
        )
    }

    #[must_use]
    pub fn can_broadcast(&self) -> bool {
        matches!(
            self.addressing_type(),
            AddressingType::Broadcast | AddressingType::Either
        )
    }
}

#[cfg(test)]
mod test_active_source {
    use super::*;

    message_test! {
        ty: ActiveSource,
        instance: Message::ActiveSource {
            address: PhysicalAddress(0x1234),
        },
        bytes: [0x12, 0x34],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ActiveSource as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(3),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_inactive_source {
    use super::*;

    message_test! {
        ty: InactiveSource,
        instance: Message::InactiveSource {
            address: PhysicalAddress(0x1234),
        },
        bytes: [0x12, 0x34],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::InactiveSource as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(3),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_routing_change {
    use super::*;

    message_test! {
        ty: RoutingChange,
        instance: Message::RoutingChange {
            original_address: PhysicalAddress(0x1234),
            new_address: PhysicalAddress(0x5678),
        },
        bytes: [0x12, 0x34, 0x56, 0x78],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::RoutingChange as u8, 0x12, 0x34, 0x56]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 4,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::RoutingChange as u8, 0x12, 0x34]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_and_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::RoutingChange as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_routing_information {
    use super::*;

    message_test! {
        ty: RoutingInformation,
        instance: Message::RoutingInformation {
            address: PhysicalAddress(0x1234),
        },
        bytes: [0x12, 0x34],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::RoutingInformation as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(3),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_set_stream_path {
    use super::*;

    message_test! {
        ty: SetStreamPath,
        instance: Message::SetStreamPath {
            address: PhysicalAddress(0x1234),
        },
        bytes: [0x12, 0x34],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::SetStreamPath as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(3),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_record_on {
    use super::*;

    message_test! {
        name: _own,
        ty: RecordOn,
        instance: Message::RecordOn {
            source: operand::RecordSource::Own,
        },
        bytes: [operand::RecordSourceType::Own as u8],
        extra: [Overfull],
    }

    message_test! {
        name: _digital,
        ty: RecordOn,
        instance: Message::RecordOn {
            source: operand::RecordSource::DigitalService(
                operand::DigitalServiceId::AribGeneric(operand::AribData {
                    transport_stream_id: 0x1234,
                    service_id: 0x5678,
                    original_network_id: 0x9ABC,
                })
            )
        },
        bytes: [
            operand::RecordSourceType::Digital as u8,
            operand::DigitalServiceBroadcastSystem::AribGeneric as u8,
            0x12,
            0x34,
            0x56,
            0x78,
            0x9A,
            0xBC
        ],
        extra: [Overfull],
    }

    message_test! {
        name: _analogue,
        ty: RecordOn,
        instance: Message::RecordOn {
            source: operand::RecordSource::AnalogueService(operand::AnalogueServiceId {
                broadcast_type: operand::AnalogueBroadcastType::Satellite,
                frequency: 0x1234,
                broadcast_system: operand::BroadcastSystem::SecamL,
            })
        },
        bytes: [
            operand::RecordSourceType::Analogue as u8,
            operand::AnalogueBroadcastType::Satellite as u8,
            0x12,
            0x34,
            operand::BroadcastSystem::SecamL as u8
        ],
        extra: [Overfull],
    }

    #[test]
    fn test_decode_missing_operands() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::RecordOn as u8]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(2),
                got: 1,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::RecordOn {
                source: operand::RecordSource::Own
            }
            .opcode(),
            Opcode::RecordOn
        );
    }
}

#[cfg(test)]
mod test_record_status {
    use super::*;

    message_test! {
        ty: RecordStatus,
        instance: Message::RecordStatus {
            status: operand::RecordStatusInfo::CurrentSource
        },
        bytes: [operand::RecordStatusInfo::CurrentSource as u8],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decode_invalid_operand() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::RecordStatus as u8, 0xFE]),
            Err(Error::InvalidValueForType {
                ty: "RecordStatusInfo",
                value: String::from("254"),
            })
        );
    }
}

#[cfg(test)]
mod test_clear_analogue_timer {
    use super::*;

    message_test! {
        ty: ClearAnalogueTimer,
        instance: Message::ClearAnalogueTimer {
            day_of_month: operand::DayOfMonth::Day1,
            month_of_year: operand::MonthOfYear::January,
            start_time: operand::Time {
                hour: operand::Hour::try_from(12).unwrap(),
                minute: operand::Minute::try_from(30).unwrap(),
            },
            duration: operand::Duration {
                hours: operand::DurationHours::try_from(99).unwrap(),
                minutes: operand::Minute::try_from(59).unwrap(),
            },
            recording_sequence: operand::RecordingSequence::SUNDAY,
            service_id: operand::AnalogueServiceId {
                broadcast_type: operand::AnalogueBroadcastType::Terrestrial,
                frequency: 0x1234,
                broadcast_system: operand::BroadcastSystem::NtscM
            },
        },
        bytes: [
            operand::DayOfMonth::Day1 as u8,
            operand::MonthOfYear::January as u8,
            0x12,
            0x30,
            0x99,
            0x59,
            constants::CEC_OP_REC_SEQ_SUNDAY,
            operand::AnalogueBroadcastType::Terrestrial as u8,
            0x12,
            0x34,
            operand::BroadcastSystem::NtscM as u8
        ],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_operand_1() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
                constants::CEC_OP_REC_SEQ_SUNDAY,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 8,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 7,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_3() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 5,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_4() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_5() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_clear_digital_timer {
    use super::*;

    message_test! {
        ty: ClearDigitalTimer,
        instance: Message::ClearDigitalTimer {
            day_of_month: operand::DayOfMonth::Day1,
            month_of_year: operand::MonthOfYear::January,
            start_time: operand::Time {
                hour: operand::Hour::try_from(12).unwrap(),
                minute: operand::Minute::try_from(30).unwrap(),
            },
            duration: operand::Duration {
                hours: operand::DurationHours::try_from(99).unwrap(),
                minutes: operand::Minute::try_from(59).unwrap(),
            },
            recording_sequence: operand::RecordingSequence::SUNDAY,
            service_id: operand::DigitalServiceId::AtscCable(
                operand::AtscData {
                    transport_stream_id: 0x1234,
                    program_number: 0x5678,
                }
            ),
        },
        bytes: [
            operand::DayOfMonth::Day1 as u8,
            operand::MonthOfYear::January as u8,
            0x12,
            0x30,
            0x99,
            0x59,
            constants::CEC_OP_REC_SEQ_SUNDAY,
            operand::DigitalServiceBroadcastSystem::AtscCable as u8,
            0x12,
            0x34,
            0x56,
            0x78,
            0,
            0,
        ],
        extra: [Empty],
    }

    #[test]
    fn test_decoding_missing_operand_1() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
                constants::CEC_OP_REC_SEQ_SUNDAY,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 8,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 7,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_3() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 5,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_4() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_5() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_clear_ext_timer {
    use super::*;

    message_test! {
        name: _phys_addr,
        ty: ClearExtTimer,
        instance: Message::ClearExtTimer {
            day_of_month: operand::DayOfMonth::Day1,
            month_of_year: operand::MonthOfYear::January,
            start_time: operand::Time {
                hour: operand::Hour::try_from(12).unwrap(),
                minute: operand::Minute::try_from(30).unwrap(),
            },
            duration: operand::Duration {
                hours: operand::DurationHours::try_from(99).unwrap(),
                minutes: operand::Minute::try_from(59).unwrap(),
            },
            recording_sequence: operand::RecordingSequence::SUNDAY,
            external_source: operand::ExternalSource::PhysicalAddress(PhysicalAddress(0x1234)),
        },
        bytes: [
            operand::DayOfMonth::Day1 as u8,
            operand::MonthOfYear::January as u8,
            0x12,
            0x30,
            0x99,
            0x59,
            constants::CEC_OP_REC_SEQ_SUNDAY,
            0x12,
            0x34,
        ],
    }

    message_test! {
        name: _plug,
        ty: ClearExtTimer,
        instance: Message::ClearExtTimer {
            day_of_month: operand::DayOfMonth::Day1,
            month_of_year: operand::MonthOfYear::January,
            start_time: operand::Time {
                hour: operand::Hour::try_from(12).unwrap(),
                minute: operand::Minute::try_from(30).unwrap(),
            },
            duration: operand::Duration {
                hours: operand::DurationHours::try_from(99).unwrap(),
                minutes: operand::Minute::try_from(59).unwrap(),
            },
            recording_sequence: operand::RecordingSequence::SUNDAY,
            external_source: operand::ExternalSource::Plug(0x56),
        },
        bytes: [
            operand::DayOfMonth::Day1 as u8,
            operand::MonthOfYear::January as u8,
            0x12,
            0x30,
            0x99,
            0x59,
            constants::CEC_OP_REC_SEQ_SUNDAY,
            0x56,
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::ClearExtTimer {
                day_of_month: operand::DayOfMonth::Day1,
                month_of_year: operand::MonthOfYear::January,
                start_time: operand::Time {
                    hour: operand::Hour::try_from(12).unwrap(),
                    minute: operand::Minute::try_from(30).unwrap(),
                },
                duration: operand::Duration {
                    hours: operand::DurationHours::try_from(99).unwrap(),
                    minutes: operand::Minute::try_from(59).unwrap(),
                },
                recording_sequence: operand::RecordingSequence::SUNDAY,
                external_source: operand::ExternalSource::Plug(0x56),
            }
            .opcode(),
            Opcode::ClearExtTimer
        );
    }

    #[test]
    fn test_decoding_missing_operand_1() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
                constants::CEC_OP_REC_SEQ_SUNDAY,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 8,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 7,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_3() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 5,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_4() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_5() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ClearExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 2,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_6() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ClearExtTimer as u8]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 1,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_set_analogue_timer {
    use super::*;

    message_test! {
        ty: SetAnalogueTimer,
        instance: Message::SetAnalogueTimer {
            day_of_month: operand::DayOfMonth::Day1,
            month_of_year: operand::MonthOfYear::January,
            start_time: operand::Time {
                hour: operand::Hour::try_from(12).unwrap(),
                minute: operand::Minute::try_from(30).unwrap(),
            },
            duration: operand::Duration {
                hours: operand::DurationHours::try_from(99).unwrap(),
                minutes: operand::Minute::try_from(59).unwrap(),
            },
            recording_sequence: operand::RecordingSequence::SUNDAY,
            service_id: operand::AnalogueServiceId {
                broadcast_type: operand::AnalogueBroadcastType::Terrestrial,
                frequency: 0x1234,
                broadcast_system: operand::BroadcastSystem::NtscM
            },
        },
        bytes: [
            operand::DayOfMonth::Day1 as u8,
            operand::MonthOfYear::January as u8,
            0x12,
            0x30,
            0x99,
            0x59,
            constants::CEC_OP_REC_SEQ_SUNDAY,
            operand::AnalogueBroadcastType::Terrestrial as u8,
            0x12,
            0x34,
            operand::BroadcastSystem::NtscM as u8
        ],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_operand_1() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
                constants::CEC_OP_REC_SEQ_SUNDAY,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 8,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 7,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_3() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 5,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_4() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_5() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetAnalogueTimer as u8,
                operand::DayOfMonth::Day1 as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(12),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_set_digital_timer {
    use super::*;

    message_test! {
        ty: SetDigitalTimer,
        instance: Message::SetDigitalTimer {
            day_of_month: operand::DayOfMonth::Day1,
            month_of_year: operand::MonthOfYear::January,
            start_time: operand::Time {
                hour: operand::Hour::try_from(12).unwrap(),
                minute: operand::Minute::try_from(30).unwrap(),
            },
            duration: operand::Duration {
                hours: operand::DurationHours::try_from(99).unwrap(),
                minutes: operand::Minute::try_from(59).unwrap(),
            },
            recording_sequence: operand::RecordingSequence::SUNDAY,
            service_id: operand::DigitalServiceId::AtscCable(
                operand::AtscData {
                    transport_stream_id: 0x1234,
                    program_number: 0x5678,
                }
            ),
        },
        bytes: [
            operand::DayOfMonth::Day1 as u8,
            operand::MonthOfYear::January as u8,
            0x12,
            0x30,
            0x99,
            0x59,
            constants::CEC_OP_REC_SEQ_SUNDAY,
            operand::DigitalServiceBroadcastSystem::AtscCable as u8,
            0x12,
            0x34,
            0x56,
            0x78,
            0,
            0,
        ],
        extra: [Empty],
    }

    #[test]
    fn test_decoding_missing_operand_1() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
                constants::CEC_OP_REC_SEQ_SUNDAY,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 8,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 7,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_3() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 5,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_4() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_5() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetDigitalTimer as u8,
                operand::DayOfMonth::Day1 as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 2,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_6() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::SetDigitalTimer as u8]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(15),
                got: 1,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_set_ext_timer {
    use super::*;

    message_test! {
        name: _phys_addr,
        ty: SetExtTimer,
        instance: Message::SetExtTimer {
            day_of_month: operand::DayOfMonth::Day1,
            month_of_year: operand::MonthOfYear::January,
            start_time: operand::Time {
                hour: operand::Hour::try_from(12).unwrap(),
                minute: operand::Minute::try_from(30).unwrap(),
            },
            duration: operand::Duration {
                hours: operand::DurationHours::try_from(99).unwrap(),
                minutes: operand::Minute::try_from(59).unwrap(),
            },
            recording_sequence: operand::RecordingSequence::SUNDAY,
            external_source: operand::ExternalSource::PhysicalAddress(PhysicalAddress(0x1234)),
        },
        bytes: [
            operand::DayOfMonth::Day1 as u8,
            operand::MonthOfYear::January as u8,
            0x12,
            0x30,
            0x99,
            0x59,
            constants::CEC_OP_REC_SEQ_SUNDAY,
            0x12,
            0x34,
        ],
    }

    message_test! {
        name: _plug,
        ty: SetExtTimer,
        instance: Message::SetExtTimer {
            day_of_month: operand::DayOfMonth::Day1,
            month_of_year: operand::MonthOfYear::January,
            start_time: operand::Time {
                hour: operand::Hour::try_from(12).unwrap(),
                minute: operand::Minute::try_from(30).unwrap(),
            },
            duration: operand::Duration {
                hours: operand::DurationHours::try_from(99).unwrap(),
                minutes: operand::Minute::try_from(59).unwrap(),
            },
            recording_sequence: operand::RecordingSequence::SUNDAY,
            external_source: operand::ExternalSource::Plug(0x56),
        },
        bytes: [
            operand::DayOfMonth::Day1 as u8,
            operand::MonthOfYear::January as u8,
            0x12,
            0x30,
            0x99,
            0x59,
            constants::CEC_OP_REC_SEQ_SUNDAY,
            0x56,
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::SetExtTimer {
                day_of_month: operand::DayOfMonth::Day1,
                month_of_year: operand::MonthOfYear::January,
                start_time: operand::Time {
                    hour: operand::Hour::try_from(12).unwrap(),
                    minute: operand::Minute::try_from(30).unwrap(),
                },
                duration: operand::Duration {
                    hours: operand::DurationHours::try_from(99).unwrap(),
                    minutes: operand::Minute::try_from(59).unwrap(),
                },
                recording_sequence: operand::RecordingSequence::SUNDAY,
                external_source: operand::ExternalSource::Plug(0x56),
            }
            .opcode(),
            Opcode::SetExtTimer
        );
    }

    #[test]
    fn test_decoding_missing_operand_1() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
                constants::CEC_OP_REC_SEQ_SUNDAY,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 8,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
                0x99,
                0x59,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 7,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_3() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
                0x12,
                0x30,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 5,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_4() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::SetExtTimer as u8,
                operand::DayOfMonth::Day1 as u8,
                operand::MonthOfYear::January as u8,
            ]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_5() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::SetExtTimer as u8, operand::DayOfMonth::Day1 as u8]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 2,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_6() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::SetExtTimer as u8]),
            Err(Error::OutOfRange {
                expected: Range::Only(vec![9, 10]),
                got: 1,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_set_timer_program_title {
    use super::*;

    message_test! {
        name: _empty,
        ty: SetTimerProgramTitle,
        instance: Message::SetTimerProgramTitle {
            title: operand::BufferOperand::from_str("").unwrap(),
        },
        bytes: [],
    }

    message_test! {
        name: _full,
        ty: SetTimerProgramTitle,
        instance: Message::SetTimerProgramTitle {
            title: operand::BufferOperand::from_str("12345678901234").unwrap(),
        },
        bytes: [
            0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x30, 0x31, 0x32, 0x33,
            0x34,
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::SetTimerProgramTitle {
                title: operand::BufferOperand::from_str("12345678901234").unwrap(),
            }
            .opcode(),
            Opcode::SetTimerProgramTitle
        );
    }
}

#[cfg(test)]
mod test_timer_cleared_status {
    use super::*;

    message_test! {
        ty: TimerClearedStatus,
        instance: Message::TimerClearedStatus {
            status: operand::TimerClearedStatusData::Cleared,
        },
        bytes: [operand::TimerClearedStatusData::Cleared as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_timer_status {
    use super::*;

    message_test! {
        ty: TimerStatus,
        instance: Message::TimerStatus {
            status: operand::TimerStatusData {
                overlap_warning: false,
                media_info: operand::MediaInfo::UnprotectedMedia,
                programmed_info: operand::TimerProgrammedInfo::Programmed(operand::ProgrammedInfo::EnoughSpace),
            },
        },
        bytes: [(operand::MediaInfo::UnprotectedMedia as u8) | 0x10 | constants::CEC_OP_PROG_INFO_ENOUGH_SPACE],
        extra: [Overfull, Empty],
    }

    message_test! {
        name: _no_duration,
        ty: TimerStatus,
        instance: Message::TimerStatus {
            status: operand::TimerStatusData {
                overlap_warning: false,
                media_info: operand::MediaInfo::UnprotectedMedia,
                programmed_info: operand::TimerProgrammedInfo::Programmed(operand::ProgrammedInfo::NotEnoughSpace {
                    duration_available: None,
                }),
            },
        },
        bytes: [(operand::MediaInfo::UnprotectedMedia as u8) | 0x10 | constants::CEC_OP_PROG_INFO_NOT_ENOUGH_SPACE],
    }

    message_test! {
        name: _duration,
        ty: TimerStatus,
        instance: Message::TimerStatus {
            status: operand::TimerStatusData {
                overlap_warning: false,
                media_info: operand::MediaInfo::UnprotectedMedia,
                programmed_info: operand::TimerProgrammedInfo::Programmed(operand::ProgrammedInfo::NotEnoughSpace {
                    duration_available: Some(operand::Duration {
                        hours: operand::DurationHours::try_from(30).unwrap(),
                        minutes: operand::Minute::try_from(45).unwrap(),
                    }),
                }),
            },
        },
        bytes: [
            (operand::MediaInfo::UnprotectedMedia as u8) | 0x10 | constants::CEC_OP_PROG_INFO_NOT_ENOUGH_SPACE,
            0x30,
            0x45
        ],
        extra: [Overfull],
    }
}

#[cfg(test)]
mod test_cec_version {
    use super::*;

    message_test! {
        ty: CecVersion,
        instance: Message::CecVersion {
            version: operand::Version::V2_0,
        },
        bytes: [operand::Version::V2_0 as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_report_physical_addr {
    use super::*;

    message_test! {
        ty: ReportPhysicalAddr,
        instance: Message::ReportPhysicalAddr {
            physical_address: PhysicalAddress(0x1234),
            device_type: operand::PrimaryDeviceType::Processor,
        },
        bytes: [0x12, 0x34, operand::PrimaryDeviceType::Processor as u8],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_operand() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ReportPhysicalAddr as u8, 0x12, 0x34]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(4),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_and_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ReportPhysicalAddr as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(4),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_set_menu_language {
    use super::*;

    message_test! {
        ty: SetMenuLanguage,
        instance: Message::SetMenuLanguage {
            language: [0x12, 0x34, 0x56],
        },
        bytes: [0x12, 0x34, 0x56],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::SetMenuLanguage as u8, 0x12, 0x34]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(4),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_bytes() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::SetMenuLanguage as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(4),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_deck_control {
    use super::*;

    message_test! {
        ty: DeckControl,
        instance: Message::DeckControl {
            mode: operand::DeckControlMode::Stop,
        },
        bytes: [operand::DeckControlMode::Stop as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_deck_status {
    use super::*;

    message_test! {
        ty: DeckStatus,
        instance: Message::DeckStatus {
            info: operand::DeckInfo::Record,
        },
        bytes: [operand::DeckInfo::Record as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_give_deck_status {
    use super::*;

    message_test! {
        ty: GiveDeckStatus,
        instance: Message::GiveDeckStatus {
            request: operand::StatusRequest::Once,
        },
        bytes: [operand::StatusRequest::Once as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_play {
    use super::*;

    message_test! {
        ty: Play,
        instance: Message::Play {
            mode: operand::PlayMode::Still,
        },
        bytes: [operand::PlayMode::Still as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_give_tuner_device_status {
    use super::*;

    message_test! {
        ty: GiveTunerDeviceStatus,
        instance: Message::GiveTunerDeviceStatus {
            request: operand::StatusRequest::Once,
        },
        bytes: [operand::StatusRequest::Once as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_select_analogue_service {
    use super::*;

    message_test! {
        ty: SelectAnalogueService,
        instance: Message::SelectAnalogueService {
            service_id: operand::AnalogueServiceId {
                broadcast_type: operand::AnalogueBroadcastType::Terrestrial,
                frequency: 0x1234,
                broadcast_system: operand::BroadcastSystem::PalBG,
            },
        },
        bytes: [
            operand::AnalogueBroadcastType::Terrestrial as u8,
            0x12,
            0x34,
            operand::BroadcastSystem::PalBG as u8
        ],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_select_digital_service {
    use super::*;

    message_test! {
        ty: SelectDigitalService,
        instance: Message::SelectDigitalService {
            service_id: operand::DigitalServiceId::AribGeneric(operand::AribData {
                transport_stream_id: 0x1234,
                service_id: 0x5678,
                original_network_id: 0xABCD,
            }),
        },
        bytes: [
            operand::DigitalServiceBroadcastSystem::AribGeneric as u8,
            0x12,
            0x34,
            0x56,
            0x78,
            0xAB,
            0xCD,
        ],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_tuner_device_status {
    use super::*;

    message_test! {
        ty: TunerDeviceStatus,
        instance: Message::TunerDeviceStatus {
            info: operand::TunerDeviceInfo {
                recording: true,
                tuner_display_info: operand::TunerDisplayInfo::Analogue,
                service_id: operand::ServiceId::Analogue(operand::AnalogueServiceId {
                    broadcast_type: operand::AnalogueBroadcastType::Terrestrial,
                    frequency: 0x1234,
                    broadcast_system: operand::BroadcastSystem::PalBG,
                }),
            },
        },
        bytes: [
            0x82,
            operand::AnalogueBroadcastType::Terrestrial as u8,
            0x12,
            0x34,
            operand::BroadcastSystem::PalBG as u8
        ],
        extra: [Empty],
    }
}

#[cfg(test)]
mod test_device_vendor_id {
    use super::*;

    message_test! {
        ty: DeviceVendorId,
        instance: Message::DeviceVendorId {
            vendor_id: crate::VendorId([0x12, 0x34, 0x56]),
        },
        bytes: [0x12, 0x34, 0x56],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_vendor_command {
    use super::*;

    message_test! {
        name: _empty,
        ty: VendorCommand,
        instance: Message::VendorCommand {
            command: operand::BufferOperand::from_str("").unwrap(),
        },
        bytes: [],
    }

    message_test! {
        name: _full,
        ty: VendorCommand,
        instance: Message::VendorCommand {
            command: operand::BufferOperand::from_str("12345678901234").unwrap(),
        },
        bytes: [
            0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x30, 0x31, 0x32, 0x33,
            0x34,
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::VendorCommand {
                command: operand::BufferOperand::from_str("12345678901234").unwrap(),
            }
            .opcode(),
            Opcode::VendorCommand
        );
    }
}

#[cfg(test)]
mod test_vendor_command_with_id {
    use super::*;

    message_test! {
        name: _empty,
        ty: VendorCommandWithId,
        instance: Message::VendorCommandWithId {
            vendor_id: crate::VendorId([0x12, 0x34, 0x56]),
            vendor_specific_data: operand::BoundedBufferOperand::<11, u8>::from_str("").unwrap(),
        },
        bytes: [0x12, 0x34, 0x56],
    }

    message_test! {
        name: _full,
        ty: VendorCommandWithId,
        instance: Message::VendorCommandWithId {
            vendor_id: crate::VendorId([0x12, 0x34, 0x56]),
            vendor_specific_data: operand::BoundedBufferOperand::<11, u8>::from_str("12345678901").unwrap(),
        },
        bytes: [
            0x12,
            0x34,
            0x56,
            b'1',
            b'2',
            b'3',
            b'4',
            b'5',
            b'6',
            b'7',
            b'8',
            b'9',
            b'0',
            b'1',
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::VendorCommandWithId {
                vendor_id: crate::VendorId([0x12, 0x34, 0x56]),
                vendor_specific_data: operand::BoundedBufferOperand::<11, u8>::from_str("")
                    .unwrap(),
            }
            .opcode(),
            Opcode::VendorCommandWithId
        );
    }

    #[test]
    fn test_decoding_missing_operand() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::VendorCommandWithId as u8]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(4),
                got: 1,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_vendor_remote_button_down {
    use super::*;

    message_test! {
        name: _empty,
        ty: VendorRemoteButtonDown,
        instance: Message::VendorRemoteButtonDown {
            rc_code: operand::BufferOperand::from_str("").unwrap(),
        },
        bytes: [],
    }

    message_test! {
        name: _full,
        ty: VendorRemoteButtonDown,
        instance: Message::VendorRemoteButtonDown {
            rc_code: operand::BufferOperand::from_str("12345678901234").unwrap(),
        },
        bytes: [
            0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x30, 0x31, 0x32, 0x33,
            0x34,
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::VendorRemoteButtonDown {
                rc_code: operand::BufferOperand::from_str("12345678901234").unwrap(),
            }
            .opcode(),
            Opcode::VendorRemoteButtonDown
        );
    }
}

#[cfg(test)]
mod test_set_osd_string {
    use super::*;

    message_test! {
        name: _empty,
        ty: SetOsdString,
        instance: Message::SetOsdString {
            display_control: operand::DisplayControl::UntilCleared,
            osd_string: operand::BoundedBufferOperand::<13, u8>::from_str("").unwrap(),
        },
        bytes: [operand::DisplayControl::UntilCleared as u8],
    }

    message_test! {
        name: _full,
        ty: SetOsdString,
        instance: Message::SetOsdString {
            display_control: operand::DisplayControl::UntilCleared,
            osd_string: operand::BoundedBufferOperand::<13, u8>::from_str("1234567890123").unwrap(),
        },
        bytes: [
            operand::DisplayControl::UntilCleared as u8,
            b'1',
            b'2',
            b'3',
            b'4',
            b'5',
            b'6',
            b'7',
            b'8',
            b'9',
            b'0',
            b'1',
            b'2',
            b'3',
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::SetOsdString {
                display_control: operand::DisplayControl::UntilCleared,
                osd_string: operand::BoundedBufferOperand::<13, u8>::from_str("").unwrap(),
            }
            .opcode(),
            Opcode::SetOsdString
        );
    }

    #[test]
    fn test_decoding_missing_operand() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::SetOsdString as u8]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(2),
                got: 1,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_set_osd_name {
    use super::*;

    message_test! {
        name: _empty,
        ty: SetOsdName,
        instance: Message::SetOsdName {
            name: operand::BufferOperand::from_str("").unwrap(),
        },
        bytes: [],
    }

    message_test! {
        name: _full,
        ty: SetOsdName,
        instance: Message::SetOsdName {
            name: operand::BufferOperand::from_str("12345678901234").unwrap(),
        },
        bytes: [
            0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x30, 0x31, 0x32, 0x33,
            0x34,
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::SetOsdName {
                name: operand::BufferOperand::from_str("12345678901234").unwrap(),
            }
            .opcode(),
            Opcode::SetOsdName
        );
    }
}

#[cfg(test)]
mod test_menu_request {
    use super::*;

    message_test! {
        ty: MenuRequest,
        instance: Message::MenuRequest {
            request_type: operand::MenuRequestType::Query,
        },
        bytes: [operand::MenuRequestType::Query as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_menu_state {
    use super::*;

    message_test! {
        ty: MenuStatus,
        instance: Message::MenuStatus {
            state: operand::MenuState::Deactivated,
        },
        bytes: [operand::MenuState::Deactivated as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_user_control_pressed {
    use super::*;

    message_test! {
        ty: UserControlPressed,
        instance: Message::UserControlPressed {
            ui_command: operand::UiCommand::Play,
        },
        bytes: [constants::CEC_OP_UI_CMD_PLAY],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_report_power_status {
    use super::*;

    message_test! {
        ty: ReportPowerStatus,
        instance: Message::ReportPowerStatus {
            status: operand::PowerStatus::ToOn,
        },
        bytes: [operand::PowerStatus::ToOn as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_feature_abort {
    use super::*;

    message_test! {
        ty: FeatureAbort,
        instance: Message::FeatureAbort {
            opcode: Opcode::FeatureAbort.into(),
            abort_reason: operand::AbortReason::IncorrectMode,
        },
        bytes: [Opcode::FeatureAbort as u8, operand::AbortReason::IncorrectMode as u8],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_operand() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::FeatureAbort as u8, Opcode::FeatureAbort as u8]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(3),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_report_audio_status {
    use super::*;

    message_test! {
        ty: ReportAudioStatus,
        instance: Message::ReportAudioStatus {
            status: operand::AudioStatus::new().with_volume(2).with_mute(true),
        },
        bytes: [0x82],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_report_short_audio_descriptor {
    use super::*;

    message_test! {
        name: _empty,
        ty: ReportShortAudioDescriptor,
        instance: Message::ReportShortAudioDescriptor {
            descriptors: operand::BoundedBufferOperand::default(),
        },
        bytes: [],
    }

    message_test! {
        name: _full,
        ty: ReportShortAudioDescriptor,
        instance: Message::ReportShortAudioDescriptor {
            descriptors: operand::BoundedBufferOperand::try_from([
                [0x01, 0x23, 0x45],
                [0x67, 0x89, 0xAB],
                [0xCD, 0xEF, 0xFE],
                [0xDC, 0xBA, 0x98]
            ].as_ref())
            .unwrap(),
        },
        bytes: [
            0x01,
            0x23,
            0x45,
            0x67,
            0x89,
            0xAB,
            0xCD,
            0xEF,
            0xFE,
            0xDC,
            0xBA,
            0x98,
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::ReportShortAudioDescriptor {
                descriptors: operand::BoundedBufferOperand::try_from(
                    [
                        [0x01, 0x23, 0x45],
                        [0x67, 0x89, 0xAB],
                        [0xCD, 0xEF, 0xFE],
                        [0xDC, 0xBA, 0x98]
                    ]
                    .as_ref()
                )
                .unwrap(),
            }
            .opcode(),
            Opcode::ReportShortAudioDescriptor
        );
    }
}

#[cfg(test)]
mod test_request_short_audio_descriptor {
    use super::*;

    message_test! {
        name: _empty,
        ty: RequestShortAudioDescriptor,
        instance: Message::RequestShortAudioDescriptor {
            descriptors: operand::BoundedBufferOperand::default(),
        },
        bytes: [],
    }

    message_test! {
        name: _full,
        ty: RequestShortAudioDescriptor,
        instance: Message::RequestShortAudioDescriptor {
            descriptors: operand::BoundedBufferOperand::try_from([
                operand::AudioFormatIdAndCode::new()
                    .with_code(1)
                    .with_id(operand::AudioFormatId::CEA861),
                operand::AudioFormatIdAndCode::new()
                    .with_code(2)
                    .with_id(operand::AudioFormatId::CEA861),
                operand::AudioFormatIdAndCode::new()
                    .with_code(3)
                    .with_id(operand::AudioFormatId::CEA861Cxt),
                operand::AudioFormatIdAndCode::new()
                    .with_code(4)
                    .with_id(operand::AudioFormatId::CEA861Cxt),
            ].as_ref())
            .unwrap(),
        },
        bytes: [0x01, 0x02, 0x43, 0x44],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::RequestShortAudioDescriptor {
                descriptors: operand::BoundedBufferOperand::try_from(
                    [
                        operand::AudioFormatIdAndCode::new()
                            .with_code(1)
                            .with_id(operand::AudioFormatId::CEA861),
                        operand::AudioFormatIdAndCode::new()
                            .with_code(2)
                            .with_id(operand::AudioFormatId::CEA861),
                        operand::AudioFormatIdAndCode::new()
                            .with_code(3)
                            .with_id(operand::AudioFormatId::CEA861Cxt),
                        operand::AudioFormatIdAndCode::new()
                            .with_code(4)
                            .with_id(operand::AudioFormatId::CEA861Cxt),
                    ]
                    .as_ref()
                )
                .unwrap(),
            }
            .opcode(),
            Opcode::RequestShortAudioDescriptor
        );
    }
}

#[cfg(test)]
mod test_set_system_audio_mode {
    use super::*;

    message_test! {
        ty: SetSystemAudioMode,
        instance: Message::SetSystemAudioMode {
            status: true,
        },
        bytes: [0x01],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_system_audio_mode_request {
    use super::*;

    message_test! {
        ty: SystemAudioModeRequest,
        instance: Message::SystemAudioModeRequest {
            physical_address: PhysicalAddress(0x1234),
        },
        bytes: [0x12, 0x34],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::SystemAudioModeRequest as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(3),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_system_audio_mode_status {
    use super::*;

    message_test! {
        ty: SystemAudioModeStatus,
        instance: Message::SystemAudioModeStatus {
            status: true,
        },
        bytes: [0x01],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_set_audio_rate {
    use super::*;

    message_test! {
        ty: SetAudioRate,
        instance: Message::SetAudioRate {
            audio_rate: operand::AudioRate::WideFast,
        },
        bytes: [operand::AudioRate::WideFast as u8],
        extra: [Overfull, Empty],
    }
}

#[cfg(test)]
mod test_cdc_message {
    use super::*;

    message_test! {
        ty: CdcMessage,
        instance: Message::CdcMessage {
            initiator: PhysicalAddress(0x0123),
            message: CdcMessage::HecRequestDeactivation {
                terminating_address1: PhysicalAddress(0x4567),
                terminating_address2: PhysicalAddress(0x89AB),
                terminating_address3: PhysicalAddress(0xCDEF)
            },
        },
        bytes: [0x01, 0x23, CdcOpcode::HecRequestDeactivation as u8, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_operand() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::CdcMessage as u8,
                0x01,
                0x23,
                CdcOpcode::HecRequestDeactivation as u8
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(10),
                got: 4,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_opcode() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::CdcMessage as u8, 0x01, 0x23]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(4),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_opcode_and_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::CdcMessage as u8, 0x01]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(4),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_report_features {
    use super::*;

    message_test! {
        name: _empty,
        ty: ReportFeatures,
        instance: Message::ReportFeatures {
            version: operand::Version::V2_0,
            device_types: operand::AllDeviceTypes::PLAYBACK,
            rc_profile: operand::RcProfile::new(
                operand::RcProfile1::Source(operand::RcProfileSource::all())),
            device_features: operand::DeviceFeatures::new(operand::DeviceFeatures1::all()),
        },
        bytes: [
            operand::Version::V2_0 as u8,
            operand::AllDeviceTypes::PLAYBACK.bits(),
            operand::RcProfileSource::all().bits(),
            operand::DeviceFeatures1::all().bits()
        ],
        extra: [Overfull],
    }

    message_test! {
        name: _extra_rc_profiles,
        ty: ReportFeatures,
        instance: Message::ReportFeatures {
            version: operand::Version::V2_0,
            device_types: operand::AllDeviceTypes::PLAYBACK,
            rc_profile: operand::RcProfile {
                rc_profile_1: operand::RcProfile1::Source(operand::RcProfileSource::all()),
                rc_profile_n: operand::BoundedBufferOperand::try_from([
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0
                ].as_ref()).unwrap(),
            },
            device_features: operand::DeviceFeatures::new(operand::DeviceFeatures1::all()),
        },
        bytes: [
            operand::Version::V2_0 as u8,
            operand::AllDeviceTypes::PLAYBACK.bits(),
            operand::RcProfileSource::all().bits() | 0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x0,
            operand::DeviceFeatures1::all().bits()
        ],
    }

    message_test! {
        name: _extra_device_features,
        ty: ReportFeatures,
        instance: Message::ReportFeatures {
            version: operand::Version::V2_0,
            device_types: operand::AllDeviceTypes::PLAYBACK,
            rc_profile: operand::RcProfile::new(operand::RcProfile1::Source(operand::RcProfileSource::all())),
            device_features: operand::DeviceFeatures {
                device_features_1: operand::DeviceFeatures1::all(),
                device_features_n: operand::BoundedBufferOperand::try_from([
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0
                ].as_ref()).unwrap(),
            },
        },
        bytes: [
            operand::Version::V2_0 as u8,
            operand::AllDeviceTypes::PLAYBACK.bits(),
            operand::RcProfileSource::all().bits(),
            operand::DeviceFeatures1::all().bits() | 0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x0,
        ],
    }

    message_test! {
        name: _balanced,
        ty: ReportFeatures,
        instance: Message::ReportFeatures {
            version: operand::Version::V2_0,
            device_types: operand::AllDeviceTypes::PLAYBACK,
            rc_profile: operand::RcProfile {
                rc_profile_1: operand::RcProfile1::Source(operand::RcProfileSource::all()),
                rc_profile_n: operand::BoundedBufferOperand::try_from([
                    0,
                    0,
                    0,
                    0,
                    0
                ].as_ref()).unwrap(),
            },
            device_features: operand::DeviceFeatures {
                device_features_1: operand::DeviceFeatures1::all(),
                device_features_n: operand::BoundedBufferOperand::try_from([
                    0,
                    0,
                    0,
                    0,
                    0
                ].as_ref()).unwrap(),
            },
        },
        bytes: [
            operand::Version::V2_0 as u8,
            operand::AllDeviceTypes::PLAYBACK.bits(),
            operand::RcProfileSource::all().bits() | 0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x0,
            operand::DeviceFeatures1::all().bits() | 0x80,
            0x80,
            0x80,
            0x80,
            0x80,
            0x0,
        ],
    }

    #[test]
    fn test_opcode() {
        assert_eq!(
            Message::ReportFeatures {
                version: operand::Version::V2_0,
                device_types: operand::AllDeviceTypes::PLAYBACK,
                rc_profile: operand::RcProfile::new(operand::RcProfile1::Source(
                    operand::RcProfileSource::all()
                )),
                device_features: operand::DeviceFeatures::new(operand::DeviceFeatures1::all()),
            }
            .opcode(),
            Opcode::ReportFeatures
        );
    }

    #[test]
    fn test_decoding_missing_operand_1() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ReportFeatures as u8,
                operand::Version::V2_0 as u8,
                operand::AllDeviceTypes::PLAYBACK.bits(),
                operand::RcProfileSource::all().bits(),
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 4,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2() {
        assert_eq!(
            Message::try_from_bytes(&[
                Opcode::ReportFeatures as u8,
                operand::Version::V2_0 as u8,
                operand::AllDeviceTypes::PLAYBACK.bits(),
            ]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_3() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ReportFeatures as u8, operand::Version::V2_0 as u8]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 2,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operands() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ReportFeatures as u8]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 1,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_request_current_latency {
    use super::*;

    message_test! {
        ty: RequestCurrentLatency,
        instance: Message::RequestCurrentLatency {
            physical_address: PhysicalAddress(0x1234),
        },
        bytes: [0x12, 0x34],
        extra: [Overfull, Empty],
    }

    #[test]
    fn test_decoding_missing_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::RequestCurrentLatency as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(3),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}

#[cfg(test)]
mod test_report_current_latency {
    use super::*;

    message_test! {
        ty: ReportCurrentLatency,
        instance: Message::ReportCurrentLatency {
            physical_address: PhysicalAddress(0x1234),
            video_latency: operand::Delay::try_from(0x56).unwrap(),
            flags: operand::LatencyFlags::new()
                .with_audio_out_compensated(operand::AudioOutputCompensated::PartialDelay)
                .with_low_latency_mode(true),
            audio_output_delay: Some(operand::Delay::try_from(0x78).unwrap()),
        },
        bytes: [0x12, 0x34, 0x56, 0x07, 0x78],
        extra: [Overfull, Empty],
    }

    message_test! {
        name: _no_delay,
        ty: ReportCurrentLatency,
        instance: Message::ReportCurrentLatency {
            physical_address: PhysicalAddress(0x1234),
            video_latency: operand::Delay::try_from(0x56).unwrap(),
            flags: operand::LatencyFlags::new()
                .with_audio_out_compensated(operand::AudioOutputCompensated::NoDelay)
                .with_low_latency_mode(true),
            audio_output_delay: None,
        },
        bytes: [0x12, 0x34, 0x56, 0x06],
    }

    #[test]
    fn test_decoding_missing_operand_1() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ReportCurrentLatency as u8, 0x12, 0x34, 0x56]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 4,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ReportCurrentLatency as u8, 0x12, 0x34]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 3,
                quantity: "bytes",
            })
        );
    }

    #[test]
    fn test_decoding_missing_operand_2_and_byte() {
        assert_eq!(
            Message::try_from_bytes(&[Opcode::ReportCurrentLatency as u8, 0x12]),
            Err(Error::OutOfRange {
                expected: Range::AtLeast(5),
                got: 2,
                quantity: "bytes",
            })
        );
    }
}
