/*
 * Copyright © 2025 Valve Software
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

use anyhow::{anyhow, bail, Result};
use linux_cec::device::{ConnectorInfo, Envelope, MessageData, PollResult, PollStatus};
use linux_cec::message::{Message, Opcode};
use linux_cec::operand::{AbortReason, OperandEncodable, PowerStatus, UiCommand};
use linux_cec::{Error, LogicalAddress, PhysicalAddress, TxError};
use std::ffi::OsStr;
use std::future::Future;
use std::io::{self, ErrorKind};
use std::mem::drop;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs::{read, read_dir, read_to_string};
use tokio::select;
use tokio::sync::broadcast::Receiver;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::spawn;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use zbus::object_server::InterfaceRef;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use crate::dbus::{CecDevice, CecDeviceSignals};
use crate::system::{SystemHandle, SystemMessage};
use crate::uinput::UInputDevice;
use crate::{ArcDevice, AsyncDevicePoller};

const LOG_ADDR_RETRIES: i32 = 20;
const WAKE_TRIES: i32 = 2;
const ACTIVE_SOURCE_DELAY: Duration = Duration::from_millis(100);
const WAKE_DELAY: Duration = Duration::from_millis(1000);
const RESUME_RECONFIG_DELAYS: [Duration; 5] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1000),
    Duration::from_millis(2000),
];
const REPLY_RETRIES: i32 = 4;

type CallbackFut<'a> = Box<dyn Future<Output = Result<()>> + Send + 'a>;
type Callback = dyn for<'a> FnOnce(&'a mut DeviceTask) -> CallbackFut<'a> + Send;

pub struct DeviceTask {
    device: ArcDevice,
    system: SystemHandle,
    token: CancellationToken,
    interface: InterfaceRef<CecDevice>,
    active_key: Option<UiCommand>,
    broadcast_channel: Receiver<SystemMessage>,
    unicast_channel: UnboundedReceiver<SystemMessage>,
    connection: Connection,
    path: OwnedObjectPath,
    log_addr_try: i32,
    awaiting_wake: bool,
    poller: AsyncDevicePoller,
    active: bool,
    connector: Option<DrmConnector>,
    audio_log_addr: LogicalAddress,
    with_logical_address_queue: Vec<Box<Callback>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DrmConnector {
    pub card: u32,
    pub connector: String,
    pub physical_address: PhysicalAddress,
}

#[derive(Debug)]
pub struct KeyRepeat {
    pub device: ArcDevice,
    pub token: CancellationToken,
    pub log_addr: LogicalAddress,
    pub key: UiCommand,
}

impl DeviceTask {
    pub const STATIC_HANDLERS: &[Opcode] = &[
        Opcode::GiveDevicePowerStatus,
        Opcode::UserControlPressed,
        Opcode::UserControlReleased,
        Opcode::SetStreamPath,
        Opcode::Standby,
        Opcode::RoutingChange,
        Opcode::RequestActiveSource,
        Opcode::FeatureAbort,
        Opcode::ActiveSource,
    ];

    pub async fn new(
        iface: InterfaceRef<CecDevice>,
        system: SystemHandle,
        broadcast_channel: Receiver<SystemMessage>,
        unicast_channel: UnboundedReceiver<SystemMessage>,
        connection: Connection,
    ) -> Result<DeviceTask> {
        let interface = iface.clone();
        let device;
        let token;
        let path;
        {
            let dbus_obj = iface.get().await;
            device = dbus_obj.device.clone();
            token = dbus_obj.token.clone();
            path = OwnedObjectPath::from(dbus_obj.dbus_path().clone());
        }
        let poller = device.lock().await.get_poller().await?;
        let connector = system
            .lock()
            .await
            .configure_dev(device.clone(), None)
            .await?;
        let mut device_task = DeviceTask {
            device,
            system,
            token,
            interface,
            active_key: None,
            broadcast_channel,
            unicast_channel,
            connection,
            path,
            log_addr_try: LOG_ADDR_RETRIES,
            awaiting_wake: false,
            poller,
            active: false,
            connector,
            audio_log_addr: LogicalAddress::Tv,
            with_logical_address_queue: Vec::new(),
        };
        device_task.configure_uinput().await?;
        Ok(device_task)
    }

    pub async fn run(mut self) -> Result<()> {
        // On startup, we should try to figure out if there's an audio system attached
        self.query_audio_system();
        if self.system.lock().await.config.request_active_source {
            self.with_logical_address(Box::new(|this| Box::new(this.pending_activate())))
                .await;
        }
        loop {
            select! {
                status = self.poller.poll(Duration::from_secs(2).try_into().unwrap()) => {
                    let Ok(status) = status else {
                        continue
                    };
                    if status == PollStatus::Destroyed {
                        let path = &self.path;
                        info!("Device {path} disconnected");
                        self.token.cancel();
                        break;
                    }
                    let Ok(results) = self
                        .device
                        .lock()
                        .await
                        .handle_status(status)
                        .await
                        .inspect_err(|e| warn!("Failed to handle status: {e}"))
                    else {
                        continue
                    };
                    for res in results {
                        if let Err(err) = self.handle_poll_result(res).await {
                            error!("Poll handling failed: {err}");
                        }
                    }
                }
                message = self.broadcast_channel.recv() => {
                    let Ok(message) = message else {
                        break;
                    };
                    if let Err(err) = self.handle_system_message(message).await {
                        error!("Message handling failed: {err}");
                    }
                }
                message = self.unicast_channel.recv() => {
                    let Some(message) = message else {
                        break;
                    };
                    if let Err(err) = self.handle_system_message(message).await {
                        error!("Message handling failed: {err}");
                    }
                }
                () = self.token.cancelled() => break,
            }
        }
        let path = self.path;
        info!("Deregistering path {path}");
        let object_server = self.connection.object_server();
        object_server.remove::<CecDevice, _>(path).await?;
        Ok(())
    }

    async fn handle_poll_result(&mut self, result: PollResult) -> Result<()> {
        match result {
            PollResult::Message(envelope) => self.handle_message(envelope).await?,
            PollResult::LostMessages(n) => warn!("Lost {n} messages!"),
            PollResult::StateChange => {
                let device = self.device.lock().await;
                let phys_addr = device
                    .get_physical_address()
                    .await
                    .unwrap_or_default()
                    .into();
                let log_addrs = device
                    .get_logical_addresses()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                let vendor_id = device
                    .get_vendor_id()
                    .await
                    .unwrap_or_default()
                    .map_or(-1, Into::<i32>::into);
                drop(device);

                let emitter = self.interface.signal_emitter();
                let mut iface = self.interface.get_mut().await;
                if iface.cached_phys_addr != phys_addr {
                    info!(
                        "Physical address changed from {:?} to {phys_addr:?}",
                        iface.cached_phys_addr
                    );
                    iface.cached_phys_addr = phys_addr;
                    iface.physical_address_changed(emitter).await?;
                }
                if iface.cached_vendor_id != vendor_id {
                    info!(
                        "Vendor ID changed from {:?} to {vendor_id:?}",
                        iface.cached_vendor_id
                    );
                    iface.cached_vendor_id = vendor_id;
                    iface.vendor_id_changed(emitter).await?;
                }
                if iface.cached_log_addrs != log_addrs {
                    info!(
                        "Logical addresses changed from {:?} to {log_addrs:?}",
                        iface.cached_log_addrs
                    );
                    iface.cached_log_addrs = log_addrs;
                    if iface.cached_log_addrs.is_empty() {
                        self.log_addr_try = LOG_ADDR_RETRIES;
                    }
                    iface.logical_addresses_changed(emitter).await?;
                    if !iface.cached_log_addrs.is_empty() {
                        drop(iface);
                        for cb in self
                            .with_logical_address_queue
                            .drain(0..)
                            .collect::<Vec<_>>()
                        {
                            let fut = cb(self);
                            let fut = Box::into_pin(fut);
                            let res = fut.await;
                            if let Err(e) = res {
                                warn!("{e}");
                            }
                        }
                        self.query_audio_system();
                        if self.system.lock().await.config.request_active_source {
                            self.with_logical_address(Box::new(|this| {
                                Box::new(this.pending_activate())
                            }))
                            .await;
                        }
                    }
                } else if log_addrs.is_empty() && phys_addr != 0xFFFF && self.log_addr_try > 0 {
                    info!("Did not get logical address, retrying registration");
                    self.log_addr_try -= 1;
                    self.connector = self
                        .system
                        .lock()
                        .await
                        .configure_dev(self.device.clone(), self.connector.as_ref())
                        .await?;
                }
            }
            _ => (),
        }
        Ok(())
    }

    // Some calls need to happen as soon as we get a logical address, but that
    // might be deferred from when we need to queue them up. Instead of having
    // a complicated series of flags to specify what we need to do when we get
    // a logical address, we instead have a complicated deferred callback
    // system. It's not simpler, but it is more extensible.
    async fn with_logical_address(&mut self, callback: Box<Callback>) {
        if self
            .device
            .lock()
            .await
            .get_logical_addresses()
            .await
            .unwrap_or_default()
            .is_empty()
        {
            self.with_logical_address_queue.push(callback);
        } else {
            let fut = Box::into_pin(callback(self));
            let _res = fut.await;
        }
    }

    async fn handle_message(&mut self, envelope: Envelope) -> Result<()> {
        let initiator = envelope.initiator;
        let destination = envelope.destination;
        debug!(
            "Got message from {initiator} ({:x}) to {destination} ({:x}): {:?}",
            initiator as u8, destination as u8, envelope.message
        );
        self.interface
            .received_message(
                initiator.into(),
                destination.into(),
                envelope.timestamp,
                envelope.message.to_bytes().as_ref(),
            )
            .await?;

        let reply = match envelope.message {
            MessageData::Valid(Message::GiveDevicePowerStatus) => Some((
                Message::ReportPowerStatus {
                    status: PowerStatus::On,
                },
                initiator,
            )),
            MessageData::Valid(Message::UserControlPressed { ui_command }) => {
                let mut buf = Vec::new();
                ui_command.to_bytes(&mut buf);
                self.interface
                    .user_control_pressed(buf.as_ref(), initiator as u8)
                    .await?;
                if let Some(uinput) = self.interface.get_mut().await.uinput.as_mut() {
                    if let Some(old_key) = self.active_key {
                        uinput.key_up(old_key)?;
                    }
                    uinput.key_down(ui_command)?;
                }
                self.active_key = Some(ui_command);
                None
            }
            MessageData::Valid(Message::UserControlReleased) => {
                self.interface
                    .user_control_released(initiator as u8)
                    .await?;
                if let Some(old_key) = self.active_key {
                    if let Some(uinput) = self.interface.get_mut().await.uinput.as_mut() {
                        uinput.key_up(old_key)?;
                    }
                    self.active_key = None;
                }
                None
            }
            MessageData::Valid(Message::SetStreamPath { address }) => {
                let this_address = self.device.lock().await.get_physical_address().await?;
                if address == this_address {
                    self.set_active(true).await?;
                    Some((
                        Message::ActiveSource {
                            address: this_address,
                        },
                        LogicalAddress::Broadcast,
                    ))
                } else {
                    self.set_active(false).await?;
                    None
                }
            }
            MessageData::Valid(Message::ActiveSource { address }) => {
                let this_address = self.device.lock().await.get_physical_address().await?;
                self.set_active(address == this_address).await?;
                None
            }
            MessageData::Valid(Message::Standby)
                if self.system.lock().await.config.allow_standby =>
            {
                if let Err(e) = self.system.suspend().await {
                    error!("Failed to standby: {e}");
                    Some((
                        Message::FeatureAbort {
                            opcode: envelope.message.opcode(),
                            abort_reason: AbortReason::IncorrectMode,
                        },
                        initiator,
                    ))
                } else {
                    None
                }
            }
            MessageData::Valid(Message::RoutingChange { new_address, .. }) => {
                let this_address = self.device.lock().await.get_physical_address().await?;
                if new_address == this_address {
                    self.awaiting_wake = false;
                    self.set_active(true).await?;
                } else {
                    self.set_active(false).await?;
                }
                None
            }
            MessageData::Valid(Message::RequestActiveSource)
                if self.awaiting_wake || self.active =>
            {
                let address = self.device.lock().await.get_physical_address().await?;
                Some((Message::ActiveSource { address }, LogicalAddress::Broadcast))
            }
            MessageData::Valid(Message::SetSystemAudioMode { status })
            | MessageData::Valid(Message::SystemAudioModeStatus { status }) => {
                if status {
                    self.audio_log_addr = envelope.initiator;
                } else {
                    self.audio_log_addr = LogicalAddress::Tv;
                }
                let emitter = self.interface.signal_emitter();
                let mut iface = self.interface.get_mut().await;
                if iface.cached_audio_log_addr != self.audio_log_addr.into() {
                    info!(
                        "Audio logical address address changed from {:?} to {:?}",
                        iface.cached_audio_log_addr, self.audio_log_addr as u8,
                    );
                    iface.cached_audio_log_addr = self.audio_log_addr.into();
                    iface.audio_logical_address_changed(emitter).await?;
                }
                None
            }
            MessageData::Valid(Message::FeatureAbort { .. }) => None,
            _ if envelope.destination != LogicalAddress::Broadcast => {
                let opcode = envelope.message.opcode();
                if let Some(handler) = self.system.get_message_handler(opcode).await {
                    handler.handle(&self.path, opcode, &envelope).await
                } else {
                    Some((
                        Message::FeatureAbort {
                            opcode: envelope.message.opcode(),
                            abort_reason: AbortReason::UnrecognizedOp,
                        },
                        initiator,
                    ))
                }
            }
            _ => None,
        };

        if let Some((reply, address)) = reply {
            for i in 0..REPLY_RETRIES {
                let Err(err) = self.device.lock().await.tx_message(&reply, address).await else {
                    break;
                };
                if i == REPLY_RETRIES - 1
                    || !matches!(
                        err,
                        Error::TxError(TxError::UnknownError | TxError::Aborted)
                    )
                {
                    return Err(err.into());
                }
            }
        }
        Ok(())
    }

    async fn wake(&mut self) -> Result<()> {
        self.device.lock().await.wake(false, false).await?;
        self.awaiting_wake = true;
        sleep(ACTIVE_SOURCE_DELAY).await;
        for _ in 0..WAKE_TRIES {
            let result = self.device.lock().await.set_active_source(None).await;
            match result {
                Ok(()) => {
                    if !self.awaiting_wake {
                        return Ok(());
                    }
                }
                Err(Error::NoLogicalAddress) => {
                    debug!("Lost logical address. Retrying configuring.");
                    match self
                        .system
                        .lock()
                        .await
                        .configure_dev(self.device.clone(), self.connector.as_ref())
                        .await
                    {
                        Ok(connector) => self.connector = connector,
                        Err(err) => {
                            if matches!(err.downcast::<Error>(), Ok(Error::Disconnected)) {
                                self.awaiting_wake = false;
                                debug!("Device was disconnected.");
                                return Err(Error::Disconnected.into());
                            }
                        }
                    }
                    continue;
                }
                Err(Error::Disconnected) => {
                    self.awaiting_wake = false;
                    result?;
                }
                Err(e) => warn!("Failed to activate source: {e}"),
            }
            sleep(WAKE_DELAY).await;
        }
        info!("TV did not respond to wake immediately");
        Ok(())
    }

    async fn reconfigure_after_resume(&mut self) {
        for (attempt, delay) in RESUME_RECONFIG_DELAYS.into_iter().enumerate() {
            sleep(delay).await;
            match self
                .system
                .lock()
                .await
                .configure_dev(self.device.clone(), self.connector.as_ref())
                .await
            {
                Ok(connector) => self.connector = connector,
                Err(err) => {
                    warn!(
                        "Failed to reconfigure CEC adapter after resume on attempt {}: {err}",
                        attempt + 1
                    );
                    continue;
                }
            }

            let device = self.device.lock().await;
            let physical_address = match device.get_physical_address().await {
                Ok(address) if address != PhysicalAddress::default() => address,
                Ok(_) => {
                    debug!(
                        "CEC adapter still has an invalid physical address after resume attempt {}",
                        attempt + 1
                    );
                    continue;
                }
                Err(err) => {
                    warn!(
                        "Failed to read CEC physical address after resume attempt {}: {err}",
                        attempt + 1
                    );
                    continue;
                }
            };
            if device
                .get_logical_addresses()
                .await
                .unwrap_or_default()
                .is_empty()
            {
                debug!(
                    "CEC adapter still has no logical address after resume attempt {}",
                    attempt + 1
                );
                continue;
            }
            match device.poll_address(LogicalAddress::Tv).await {
                Ok(()) => {
                    info!(
                        "CEC adapter at {physical_address} reconfigured after resume; TV reachable on attempt {}",
                        attempt + 1
                    );
                    return;
                }
                Err(err) => debug!(
                    "CEC adapter reconfigured after resume attempt {}, but TV is unreachable: {err}",
                    attempt + 1
                ),
            }
        }

        warn!(
            "CEC adapter reconfigured after resume, but TV remained unreachable after {} attempts",
            RESUME_RECONFIG_DELAYS.len()
        );
    }

    async fn handle_system_message(&mut self, message: SystemMessage) -> Result<()> {
        match message {
            SystemMessage::Wake {
                wake_tv,
                from_standby,
            } => {
                if from_standby {
                    self.reconfigure_after_resume().await;
                }
                if wake_tv {
                    self.with_logical_address(Box::new(|task| {
                        Box::new(async move {
                            task.wake().await?;
                            Ok(())
                        })
                    }))
                    .await;
                }
                if from_standby {
                    let device = self.device.clone();
                    self.with_logical_address(Box::new(|_| {
                        Box::new(async move {
                            device
                                .lock()
                                .await
                                .tx_message(
                                    &Message::ReportPowerStatus {
                                        status: PowerStatus::On,
                                    },
                                    LogicalAddress::Broadcast,
                                )
                                .await?;
                            Ok(())
                        })
                    }))
                    .await;

                    if self.system.lock().await.config.request_active_source {
                        self.with_logical_address(Box::new(|this| {
                            Box::new(this.pending_activate())
                        }))
                        .await;
                    }
                }
                Ok(())
            }
            SystemMessage::Standby { standby_tv, force } => {
                let inactive_source_on_suspend =
                    self.system.lock().await.config.inactive_source_on_suspend;
                let device = self.device.lock().await;
                if inactive_source_on_suspend {
                    match device.get_physical_address().await {
                        Ok(address) => {
                            if let Err(err) = device
                                .tx_message(
                                    &Message::InactiveSource { address },
                                    LogicalAddress::Tv,
                                )
                                .await
                            {
                                warn!("Failed to send Inactive Source to TV (0): {err}");
                            }
                        }
                        Err(err) => warn!(
                            "Failed to get physical address for Inactive Source to TV (0): {err}"
                        ),
                    }
                }
                if force || (self.active && standby_tv) {
                    if let Err(err) = device.standby(LogicalAddress::Tv).await {
                        warn!("Failed to send Standby to TV (0): {err}");
                    }
                }
                Ok(())
            }
            SystemMessage::ReloadConfig => {
                self.connector = self
                    .system
                    .lock()
                    .await
                    .configure_dev(self.device.clone(), self.connector.as_ref())
                    .await?;
                self.configure_uinput().await?;
                Ok(())
            }
            SystemMessage::ReconfigureConnector(conn_info) => {
                match conn_info {
                    ConnectorInfo::DrmConnector { .. } => {
                        if let Some(connector) = self.connector.as_ref() {
                            if connector.to_connector_info().await? != conn_info {
                                return Ok(());
                            }
                        }
                    }
                    ConnectorInfo::None => (),
                    _ => return Ok(()),
                }
                self.connector = self
                    .system
                    .lock()
                    .await
                    .configure_dev(self.device.clone(), self.connector.as_ref())
                    .await?;
                Ok(())
            }
        }
    }

    async fn configure_uinput(&mut self) -> Result<()> {
        let mut interface = self.interface.get_mut().await;
        let system = self.system.lock().await;
        interface.uinput = None; // Drop old UInputDevice before opening a new one
        if system.config.mappings.is_empty() || !system.config.uinput {
            return Ok(());
        }

        let mappings = system.config.mappings.clone();
        drop(system);

        let adapter_name = self.device.lock().await.get_adapter_name().await?;
        let adapter_name = adapter_name.to_string_lossy();
        let mut uinput =
            UInputDevice::new().inspect_err(|e| warn!("Failed to open uinput device: {e}"))?;
        uinput.set_mappings(mappings)?;
        uinput.set_name(format!("cecd {adapter_name}"))?;
        uinput.open()?;
        interface.uinput = Some(uinput);
        Ok(())
    }

    async fn set_active(&mut self, active: bool) -> Result<()> {
        if self.active == active {
            return Ok(());
        }
        info!(
            "Device {} became {}",
            self.path,
            if active { "active" } else { "inactive" }
        );
        self.active = active;
        let emitter = self.interface.signal_emitter();
        let mut iface = self.interface.get_mut().await;
        if iface.cached_active != active {
            iface.cached_active = active;
            iface.active_changed(emitter).await?;
        }
        Ok(())
    }

    async fn pending_activate(&mut self) -> Result<()> {
        let device = self.device.clone();
        spawn(async move {
            let device = device.lock().await;
            if device
                .get_logical_addresses()
                .await
                .unwrap_or_default()
                .is_empty()
            {
                return;
            }
            if let Err(e) = device
                .tx_message(&Message::RequestActiveSource, LogicalAddress::Broadcast)
                .await
            {
                if !matches!(e, Error::TxError(TxError::Timeout)) {
                    info!("Couldn't query active source: {e}");
                }
            }
        });
        self.set_active(true).await?;
        Ok(())
    }

    fn query_audio_system(&self) {
        let device = self.device.clone();
        spawn(async move {
            let device = device.lock().await;
            if device
                .get_logical_addresses()
                .await
                .unwrap_or_default()
                .is_empty()
            {
                return;
            }
            if let Err(e) = device
                .tx_message(
                    &Message::GiveSystemAudioModeStatus,
                    LogicalAddress::AudioSystem,
                )
                .await
            {
                if !matches!(e, Error::TxError(TxError::Nack)) {
                    info!("Couldn't query audio system: {e}");
                }
            }
        });
    }
}

impl DrmConnector {
    pub async fn from_drm_path(path: impl AsRef<Path>) -> Result<Option<DrmConnector>> {
        let path = path.as_ref();
        if !path.starts_with(DrmConnector::drm_base()) {
            bail!("Not a DRM path");
        }

        let (card, connector) = path
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|name| name.split_once('-'))
            .ok_or(anyhow!("Invalid path"))?;
        let card = card.strip_prefix("card").unwrap_or(card).parse()?;

        let status = match read_to_string(path.join("status")).await {
            Ok(status) => status,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if status.trim() != "connected" {
            return Ok(None);
        }

        let edid = match read(path.join("edid")).await {
            Ok(edid) => edid,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(physical_address) = DrmConnector::parse_hdmi_edid_pa(&edid) else {
            return Ok(None);
        };
        if !physical_address.is_valid() || physical_address.is_root() {
            return Ok(None);
        }

        Ok(Some(DrmConnector {
            card,
            connector: connector.to_string(),
            physical_address,
        }))
    }

    pub async fn reload_physical_address(&mut self) -> Result<()> {
        let path = self.drm_path();
        let edid = read(path.join("edid")).await?;
        let physical_address =
            DrmConnector::parse_hdmi_edid_pa(&edid).ok_or(anyhow!("Failed to parse EDID"))?;
        self.physical_address = physical_address;
        Ok(())
    }

    #[cfg(not(test))]
    fn drm_base() -> PathBuf {
        PathBuf::from("/sys/class/drm")
    }

    #[cfg(test)]
    fn drm_base() -> PathBuf {
        PathBuf::from("data/test")
    }

    pub fn drm_path(&self) -> PathBuf {
        DrmConnector::drm_base().join(format!("card{}-{}", self.card, self.connector))
    }

    fn parse_hdmi_edid_pa(edid: &[u8]) -> Option<PhysicalAddress> {
        const HDMI_OUI: &[u8] = &[0x03, 0x0C, 0x00];
        let mut block = 128;
        // We ignore a lot of the spec and let through a bunch of bad EDIDs.
        // As it turns out, a lot of vendors ship bad EDIDs. We want to be as
        // permissive as possible without misparsing things that are
        // definitively what we're looking for.
        while block + 128 <= edid.len() {
            if edid[block] != 2 && edid[block + 1] < 3 {
                // Requires CTA EDID version 3 or newer
                block += 128;
                continue;
            }
            let end = block
                + (if edid[block + 2] >= 4 {
                    usize::min(edid[block + 2] as usize, 127)
                } else {
                    127
                });
            let mut offset = block + 4;
            while offset + 1 < edid.len() && offset < end {
                let header = edid[offset];
                let size = ((header & 0x1F) + 1) as usize;
                if size < 6 {
                    offset += size;
                    continue;
                }
                if offset + size > end {
                    break;
                }
                if (header & 0xE0) != 0x60 {
                    // Not a vendor specific data block
                    offset += size;
                    continue;
                }
                if &edid[offset + 1..offset + 4] != HDMI_OUI {
                    // Not an HDMI block
                    offset += size;
                    continue;
                }
                return Some(PhysicalAddress::from(
                    (u16::from(edid[offset + 4]) << 8) | u16::from(edid[offset + 5]),
                ));
            }
            block += 128;
        }
        None
    }

    pub async fn to_connector_info(&self) -> Result<ConnectorInfo> {
        let connector_id = read_to_string(self.drm_path().join("connector_id")).await?;
        let connector_id = connector_id.trim().parse()?;
        Ok(ConnectorInfo::DrmConnector {
            card_no: self.card,
            connector_id,
        })
    }

    pub async fn from_connector_info(
        info: ConnectorInfo,
        physical_address: PhysicalAddress,
    ) -> Result<DrmConnector> {
        let ConnectorInfo::DrmConnector {
            card_no,
            connector_id,
        } = info
        else {
            bail!("Not a DRM connector");
        };
        let mut dir = read_dir(DrmConnector::drm_base().join(format!("card{card_no}"))).await?;
        let card_prefix = format!("card{card_no}-");
        while let Some(entry) = dir.next_entry().await? {
            let filename = entry.file_name();
            if !filename.to_string_lossy().starts_with(&card_prefix)
                || !entry.metadata().await?.is_dir()
            {
                continue;
            }
            let this_connector_id = read_to_string(entry.path().join("connector_id")).await?;
            let Ok(this_connector_id) = this_connector_id.trim().parse::<u32>() else {
                debug!(
                    "Card {} contains invalid connector id {this_connector_id}",
                    filename.display()
                );
                continue;
            };
            if this_connector_id == connector_id {
                let connector = filename
                    .to_str()
                    .and_then(|name| name.split_once('-'))
                    .ok_or(anyhow!("Invalid path"))?
                    .1;
                return Ok(DrmConnector {
                    card: card_no,
                    connector: connector.to_string(),
                    physical_address,
                });
            }
        }
        Err(io::Error::new(ErrorKind::NotFound, "Connector not found").into())
    }
}

impl KeyRepeat {
    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    fn delay(&self) -> impl Future<Output = ()> {
        // Recommended interval of 450ms is per H14b CEC 13.13.3,
        // starting at the beginning of message transmission
        sleep(Duration::from_millis(450))
    }

    #[cfg(test)]
    async fn delay(&self) {
        let key_repeat = self.device.lock().await.key_repeat.clone();
        key_repeat.notified().await
    }

    pub async fn run(self) -> Result<()> {
        loop {
            let delay = self.delay();
            self.device
                .lock()
                .await
                .press_user_control(self.key, self.log_addr)
                .await?;
            select! {
                () = self.token.cancelled() => break,
                () = delay => continue,
            }
        }
        Ok(self
            .device
            .lock()
            .await
            .release_user_control(self.log_addr)
            .await?)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use cecd_proxy::CecDevice1Proxy;
    use input_linux::{EventKind, Key, KeyEvent, KeyState};
    use linux_cec::device::Capabilities;
    use linux_cec::message::Opcode;
    use linux_cec::{LogicalAddressType, PhysicalAddress};
    use std::iter::repeat_n;
    use std::num::ParseIntError;
    use std::time::Duration;
    use tokio_stream::StreamExt;

    use crate::config::Config;
    use crate::testing::{rx_message, setup_basic_test, setup_dbus_test, tx_message, wait_timeout};

    #[tokio::test]
    async fn test_tx_basic() {
        let test = setup_basic_test().await.unwrap();
        assert_eq!(test.proxy.physical_address().await.unwrap(), 0x1000);
        assert_eq!(
            test.proxy.logical_addresses().await.unwrap(),
            &[u8::from(LogicalAddress::PlaybackDevice1)]
        );

        test.proxy.standby(LogicalAddress::Tv.into()).await.unwrap();
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::Standby {}, LogicalAddress::Tv))
        );
    }

    #[tokio::test]
    async fn test_system_message_standby() {
        let test = setup_basic_test().await.unwrap();
        tx_message(
            &test.dev,
            Message::RoutingChange {
                new_address: PhysicalAddress::from(0x1000),
                original_address: PhysicalAddress::from(0x0000),
            },
            LogicalAddress::Tv,
        )
        .await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        {
            let dev = interface.get_mut().await;
            dev.send_system_message(SystemMessage::Standby {
                standby_tv: true,
                force: false,
            })
            .await
            .unwrap();
        }
        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::InactiveSource {
                    address: PhysicalAddress::from(0x1000)
                },
                LogicalAddress::Tv
            )
        );
        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (Message::Standby {}, LogicalAddress::Tv)
        );
        assert!(rx_message(&test.dev).await.is_none());
    }

    #[tokio::test]
    async fn test_system_message_standby_without_inactive_source() {
        let test = setup_basic_test().await.unwrap();
        test.system.lock().await.config.inactive_source_on_suspend = false;
        tx_message(
            &test.dev,
            Message::RoutingChange {
                new_address: PhysicalAddress::from(0x1000),
                original_address: PhysicalAddress::from(0x0000),
            },
            LogicalAddress::Tv,
        )
        .await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        {
            let dev = interface.get_mut().await;
            dev.send_system_message(SystemMessage::Standby {
                standby_tv: true,
                force: false,
            })
            .await
            .unwrap();
        }
        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (Message::Standby {}, LogicalAddress::Tv)
        );
        assert!(rx_message(&test.dev).await.is_none());
    }

    #[tokio::test]
    async fn test_system_message_standby_inactive() {
        let test = setup_basic_test().await.unwrap();
        tx_message(
            &test.dev,
            Message::RoutingChange {
                new_address: PhysicalAddress::from(0x2000),
                original_address: PhysicalAddress::from(0x1000),
            },
            LogicalAddress::Tv,
        )
        .await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        {
            let dev = interface.get_mut().await;
            dev.send_system_message(SystemMessage::Standby {
                standby_tv: true,
                force: false,
            })
            .await
            .unwrap();
        }
        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::InactiveSource {
                    address: PhysicalAddress::from(0x1000)
                },
                LogicalAddress::Tv
            )
        );
        assert!(rx_message(&test.dev).await.is_none());
    }

    #[tokio::test]
    async fn test_system_message_standby_inactive_forced() {
        let test = setup_basic_test().await.unwrap();
        tx_message(
            &test.dev,
            Message::RoutingChange {
                new_address: PhysicalAddress::from(0x2000),
                original_address: PhysicalAddress::from(0x1000),
            },
            LogicalAddress::Tv,
        )
        .await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        {
            let dev = interface.get_mut().await;
            dev.send_system_message(SystemMessage::Standby {
                standby_tv: true,
                force: true,
            })
            .await
            .unwrap();
        }
        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::InactiveSource {
                    address: PhysicalAddress::from(0x1000)
                },
                LogicalAddress::Tv
            )
        );
        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (Message::Standby {}, LogicalAddress::Tv)
        );
        assert!(rx_message(&test.dev).await.is_none());
    }

    #[tokio::test]
    async fn test_system_message_standby_no_sleep() {
        let test = setup_basic_test().await.unwrap();
        tx_message(
            &test.dev,
            Message::RoutingChange {
                new_address: PhysicalAddress::from(0x1000),
                original_address: PhysicalAddress::from(0x0000),
            },
            LogicalAddress::Tv,
        )
        .await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        {
            let dev = interface.get_mut().await;
            dev.send_system_message(SystemMessage::Standby {
                standby_tv: false,
                force: false,
            })
            .await
            .unwrap();
        }
        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::InactiveSource {
                    address: PhysicalAddress::from(0x1000)
                },
                LogicalAddress::Tv
            )
        );
        assert!(rx_message(&test.dev).await.is_none());
    }

    #[tokio::test]
    async fn test_rx_abort() {
        let test = setup_basic_test().await.unwrap();
        tx_message(&test.dev, Message::RecordOff {}, LogicalAddress::Tv).await;

        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::FeatureAbort {
                    opcode: Opcode::RecordOff as u8,
                    abort_reason: AbortReason::UnrecognizedOp,
                },
                LogicalAddress::Tv
            )
        );
    }

    #[tokio::test]
    async fn test_abort_no_abort() {
        let test = setup_basic_test().await.unwrap();
        tx_message(
            &test.dev,
            Message::FeatureAbort {
                opcode: Opcode::FeatureAbort as u8,
                abort_reason: AbortReason::UnrecognizedOp,
            },
            LogicalAddress::Tv,
        )
        .await;

        assert_eq!(rx_message(&test.dev).await, None);
    }

    #[tokio::test]
    async fn test_give_device_power_status() {
        let test = setup_basic_test().await.unwrap();
        tx_message(
            &test.dev,
            Message::GiveDevicePowerStatus {},
            LogicalAddress::Tv,
        )
        .await;

        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::ReportPowerStatus {
                    status: PowerStatus::On,
                },
                LogicalAddress::Tv
            )
        );
    }

    #[tokio::test]
    async fn test_key_repeat_none() {
        let mut test = setup_basic_test().await.unwrap();
        let mut buf = Vec::new();
        UiCommand::Select.to_bytes(&mut buf);
        test.proxy
            .press_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.proxy
            .release_user_control(LogicalAddress::Tv.into())
            .await
            .unwrap();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Select
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_key_press_once() {
        let mut test = setup_basic_test().await.unwrap();
        let mut buf = Vec::new();
        UiCommand::Select.to_bytes(&mut buf);
        test.proxy
            .press_once_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.dev.lock().await.key_repeat.notify_one();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Select
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_key_repeat() {
        let mut test = setup_basic_test().await.unwrap();
        let mut buf = Vec::new();
        UiCommand::Select.to_bytes(&mut buf);
        test.proxy
            .press_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.dev.lock().await.key_repeat.notify_one();
        test.proxy
            .release_user_control(LogicalAddress::Tv.into())
            .await
            .unwrap();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Select
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Select
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_key_double_press() {
        let mut test = setup_basic_test().await.unwrap();
        let mut buf = Vec::new();
        UiCommand::Select.to_bytes(&mut buf);
        test.proxy
            .press_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.proxy
            .press_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.proxy
            .release_user_control(LogicalAddress::Tv.into())
            .await
            .unwrap();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Select
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_key_change() {
        let mut test = setup_basic_test().await.unwrap();
        let mut buf = Vec::new();
        UiCommand::Select.to_bytes(&mut buf);
        test.proxy
            .press_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        let mut buf = Vec::new();
        UiCommand::Back.to_bytes(&mut buf);
        test.proxy
            .press_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.proxy
            .release_user_control(LogicalAddress::Tv.into())
            .await
            .unwrap();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Select
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Back
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_key_change_once() {
        let mut test = setup_basic_test().await.unwrap();
        let mut buf = Vec::new();
        UiCommand::Select.to_bytes(&mut buf);
        test.proxy
            .press_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        let mut buf = Vec::new();
        UiCommand::Back.to_bytes(&mut buf);
        test.proxy
            .press_once_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Select
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Back
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_key_not_changed_once() {
        let mut test = setup_basic_test().await.unwrap();
        let mut buf = Vec::new();
        UiCommand::Select.to_bytes(&mut buf);
        test.proxy
            .press_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.proxy
            .press_once_user_control(&buf, LogicalAddress::Tv.into())
            .await
            .unwrap();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Select
                },
                LogicalAddress::Tv
            ))
        );
        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_key_release_unmatched() {
        let mut test = setup_basic_test().await.unwrap();
        test.proxy
            .release_user_control(LogicalAddress::Tv.into())
            .await
            .unwrap();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((Message::UserControlReleased {}, LogicalAddress::Tv))
        );
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_mapped_keys() {
        async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
            let mut dev = dev.lock().await;
            dev.set_caps(Capabilities::LOG_ADDRS | Capabilities::TRANSMIT);
            dev.set_phys_addr(PhysicalAddress::from(0x1000)).await;
            Ok(())
        }
        let config = Config {
            uinput: true,
            mappings: [(UiCommand::Enter, Key::Enter)].into(),
            logical_address: LogicalAddressType::Playback,
            ..Config::default()
        };
        let test = setup_dbus_test(cb, Some(config)).await.unwrap();

        test.dev
            .lock()
            .await
            .queue_rx_message(
                Message::UserControlPressed {
                    ui_command: UiCommand::Enter,
                },
                LogicalAddress::Tv,
            )
            .await;
        test.dev
            .lock()
            .await
            .queue_rx_message(Message::UserControlReleased {}, LogicalAddress::Tv)
            .await;
        let notify = test.dev.lock().await.rx_queue_empty().await.unwrap();
        notify.notified().await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        let mut dbus_obj = interface.get_mut().await;
        let uinput = dbus_obj.uinput.as_mut().unwrap();
        let event = uinput.get_next_event().unwrap();
        let key = KeyEvent::try_from(event).unwrap();
        assert_eq!(key.key, Key::Enter);
        assert_eq!(key.value, KeyState::PRESSED);

        assert_eq!(
            uinput.get_next_event().unwrap().kind,
            EventKind::Synchronize
        );

        let event = uinput.get_next_event().unwrap();
        let key = KeyEvent::try_from(event).unwrap();
        assert_eq!(key.key, Key::Enter);
        assert_eq!(key.value, KeyState::RELEASED);

        assert_eq!(
            uinput.get_next_event().unwrap().kind,
            EventKind::Synchronize
        );
    }

    #[tokio::test]
    async fn test_unmapped_keys() {
        async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
            let mut dev = dev.lock().await;
            dev.set_caps(Capabilities::LOG_ADDRS | Capabilities::TRANSMIT);
            dev.set_phys_addr(PhysicalAddress::from(0x1000)).await;
            Ok(())
        }
        let config = Config {
            uinput: true,
            mappings: [(UiCommand::Enter, Key::Enter)].into(),
            logical_address: LogicalAddressType::Playback,
            ..Config::default()
        };
        let test = setup_dbus_test(cb, Some(config)).await.unwrap();

        test.dev
            .lock()
            .await
            .queue_rx_message(
                Message::UserControlPressed {
                    ui_command: UiCommand::Back,
                },
                LogicalAddress::Tv,
            )
            .await;
        test.dev
            .lock()
            .await
            .queue_rx_message(Message::UserControlReleased {}, LogicalAddress::Tv)
            .await;
        let notify = test.dev.lock().await.rx_queue_empty().await.unwrap();
        notify.notified().await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        let mut dbus_obj = interface.get_mut().await;
        let uinput = dbus_obj.uinput.as_mut().unwrap();
        assert!(uinput.get_next_event().is_none());
    }

    #[tokio::test]
    async fn test_mapped_keys_change() {
        async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
            let mut dev = dev.lock().await;
            dev.set_caps(Capabilities::LOG_ADDRS | Capabilities::TRANSMIT);
            dev.set_phys_addr(PhysicalAddress::from(0x1000)).await;
            Ok(())
        }
        let config = Config {
            uinput: true,
            mappings: [(UiCommand::Enter, Key::Enter), (UiCommand::Back, Key::Exit)].into(),
            logical_address: LogicalAddressType::Playback,
            ..Config::default()
        };
        let test = setup_dbus_test(cb, Some(config)).await.unwrap();

        test.dev
            .lock()
            .await
            .queue_rx_message(
                Message::UserControlPressed {
                    ui_command: UiCommand::Enter,
                },
                LogicalAddress::Tv,
            )
            .await;
        test.dev
            .lock()
            .await
            .queue_rx_message(
                Message::UserControlPressed {
                    ui_command: UiCommand::Back,
                },
                LogicalAddress::Tv,
            )
            .await;
        test.dev
            .lock()
            .await
            .queue_rx_message(Message::UserControlReleased {}, LogicalAddress::Tv)
            .await;
        let notify = test.dev.lock().await.rx_queue_empty().await.unwrap();
        notify.notified().await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        let mut dbus_obj = interface.get_mut().await;
        let uinput = dbus_obj.uinput.as_mut().unwrap();
        let event = uinput.get_next_event().unwrap();
        let key = KeyEvent::try_from(event).unwrap();
        assert_eq!(key.key, Key::Enter);
        assert_eq!(key.value, KeyState::PRESSED);

        assert_eq!(
            uinput.get_next_event().unwrap().kind,
            EventKind::Synchronize
        );

        let event = uinput.get_next_event().unwrap();
        let key = KeyEvent::try_from(event).unwrap();
        assert_eq!(key.key, Key::Enter);
        assert_eq!(key.value, KeyState::RELEASED);

        assert_eq!(
            uinput.get_next_event().unwrap().kind,
            EventKind::Synchronize
        );

        let event = uinput.get_next_event().unwrap();
        let key = KeyEvent::try_from(event).unwrap();
        assert_eq!(key.key, Key::Exit);
        assert_eq!(key.value, KeyState::PRESSED);

        assert_eq!(
            uinput.get_next_event().unwrap().kind,
            EventKind::Synchronize
        );

        let event = uinput.get_next_event().unwrap();
        let key = KeyEvent::try_from(event).unwrap();
        assert_eq!(key.key, Key::Exit);
        assert_eq!(key.value, KeyState::RELEASED);

        assert_eq!(
            uinput.get_next_event().unwrap().kind,
            EventKind::Synchronize
        );
    }

    #[tokio::test]
    async fn test_active_changed() {
        let test = setup_basic_test().await.unwrap();
        tx_message(
            &test.dev,
            Message::RoutingChange {
                new_address: PhysicalAddress::from(0x1000),
                original_address: PhysicalAddress::from(0x0),
            },
            LogicalAddress::Tv,
        )
        .await;

        let proxy = CecDevice1Proxy::builder(&test.connection)
            .path("/com/steampowered/CecDaemon1/Devices/Null")
            .unwrap()
            .build()
            .await
            .unwrap();

        let mut receiver = proxy.receive_active_changed().await;
        while let Ok(Some(signal)) = wait_timeout(receiver.next(), Duration::from_millis(100)).await
        {
            assert!(signal.get().await.unwrap());
        }
        assert!(proxy.active().await.unwrap());

        tx_message(
            &test.dev,
            Message::RoutingChange {
                new_address: PhysicalAddress::from(0x2000),
                original_address: PhysicalAddress::from(0x1000),
            },
            LogicalAddress::Tv,
        )
        .await;

        assert!(!receiver.next().await.unwrap().get().await.unwrap());
        assert!(!proxy.active().await.unwrap());

        tx_message(
            &test.dev,
            Message::RoutingChange {
                new_address: PhysicalAddress::from(0x1000),
                original_address: PhysicalAddress::from(0x2000),
            },
            LogicalAddress::Tv,
        )
        .await;

        assert!(receiver.next().await.unwrap().get().await.unwrap());
        assert!(proxy.active().await.unwrap());
    }

    #[tokio::test]
    async fn test_no_audio_system() {
        let test = setup_basic_test().await.unwrap();

        let proxy = CecDevice1Proxy::builder(&test.connection)
            .path("/com/steampowered/CecDaemon1/Devices/Null")
            .unwrap()
            .build()
            .await
            .unwrap();

        assert_eq!(proxy.audio_logical_address().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_audio_system_later() {
        let test = setup_basic_test().await.unwrap();

        let proxy = CecDevice1Proxy::builder(&test.connection)
            .path("/com/steampowered/CecDaemon1/Devices/Null")
            .unwrap()
            .build()
            .await
            .unwrap();

        assert_eq!(
            proxy.audio_logical_address().await.unwrap(),
            LogicalAddress::Tv.into()
        );
        let mut receiver = proxy.receive_audio_logical_address_changed().await;
        let _ = wait_timeout(receiver.next(), Duration::from_millis(10)).await;

        tx_message(
            &test.dev,
            Message::SystemAudioModeStatus { status: true },
            LogicalAddress::AudioSystem,
        )
        .await;

        let signal = wait_timeout(receiver.next(), Duration::from_millis(100))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            signal.get().await.unwrap(),
            LogicalAddress::AudioSystem.into()
        );
        assert_eq!(
            proxy.audio_logical_address().await.unwrap(),
            LogicalAddress::AudioSystem.into()
        );
    }

    #[tokio::test]
    async fn test_audio_system_later_removed() {
        let test = setup_basic_test().await.unwrap();

        let proxy = CecDevice1Proxy::builder(&test.connection)
            .path("/com/steampowered/CecDaemon1/Devices/Null")
            .unwrap()
            .build()
            .await
            .unwrap();

        assert_eq!(
            proxy.audio_logical_address().await.unwrap(),
            LogicalAddress::Tv.into()
        );
        let mut receiver = proxy.receive_audio_logical_address_changed().await;
        let _ = wait_timeout(receiver.next(), Duration::from_millis(10)).await;

        tx_message(
            &test.dev,
            Message::SystemAudioModeStatus { status: true },
            LogicalAddress::AudioSystem,
        )
        .await;

        let signal = wait_timeout(receiver.next(), Duration::from_millis(100))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            signal.get().await.unwrap(),
            LogicalAddress::AudioSystem.into()
        );
        assert_eq!(
            proxy.audio_logical_address().await.unwrap(),
            LogicalAddress::AudioSystem.into()
        );

        tx_message(
            &test.dev,
            Message::SystemAudioModeStatus { status: false },
            LogicalAddress::AudioSystem,
        )
        .await;

        let signal = wait_timeout(receiver.next(), Duration::from_millis(100))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(signal.get().await.unwrap(), LogicalAddress::Tv.into());
        assert_eq!(
            proxy.audio_logical_address().await.unwrap(),
            LogicalAddress::Tv.into()
        );
    }

    #[tokio::test]
    async fn test_wake_from_standby_not_deferred() {
        let test = setup_basic_test().await.unwrap();
        assert_eq!(
            test.proxy.logical_addresses().await.unwrap(),
            &[u8::from(LogicalAddress::PlaybackDevice1)]
        );

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        {
            let dev = interface.get_mut().await;
            dev.send_system_message(SystemMessage::Wake {
                wake_tv: false,
                from_standby: true,
            })
            .await
            .unwrap();
        }

        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::ReportPowerStatus {
                    status: PowerStatus::On
                },
                LogicalAddress::Broadcast
            )
        );
    }

    #[tokio::test]
    async fn test_wake_from_standby_restores_logical_address() {
        let test = setup_basic_test().await.unwrap();

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        {
            let dev = interface.get_mut().await;
            dev.device
                .lock()
                .await
                .clear_logical_addresses()
                .await
                .unwrap();
            assert_eq!(
                dev.device
                    .lock()
                    .await
                    .get_logical_addresses()
                    .await
                    .unwrap(),
                &[]
            );
            dev.send_system_message(SystemMessage::Wake {
                wake_tv: false,
                from_standby: true,
            })
            .await
            .unwrap();
        }

        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::ReportPowerStatus {
                    status: PowerStatus::On
                },
                LogicalAddress::Broadcast
            )
        );
        assert_eq!(
            test.dev.lock().await.get_logical_addresses().await.unwrap(),
            &[LogicalAddress::PlaybackDevice1]
        );
    }

    #[tokio::test]
    async fn test_wake_from_standby_retries_reconfiguration_until_tv_reachable() {
        let test = setup_basic_test().await.unwrap();
        test.dev.lock().await.queue_poll_result(false).await;
        test.dev.lock().await.queue_poll_result(true).await;

        let interface: InterfaceRef<CecDevice> = test
            .connection
            .object_server()
            .interface("/com/steampowered/CecDaemon1/Devices/Null")
            .await
            .unwrap();
        {
            let dev = interface.get_mut().await;
            dev.send_system_message(SystemMessage::Wake {
                wake_tv: false,
                from_standby: true,
            })
            .await
            .unwrap();
        }

        assert_eq!(
            rx_message(&test.dev).await.unwrap(),
            (
                Message::ReportPowerStatus {
                    status: PowerStatus::On
                },
                LogicalAddress::Broadcast
            )
        );
    }

    #[test]
    fn test_parse_edid_missing() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 80
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_missing_header() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x00, 0x00, 0x00, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_early() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x00, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(
            DrmConnector::parse_hdmi_edid_pa(&edid),
            Some(PhysicalAddress::from(0x1234))
        );
    }

    #[test]
    fn test_parse_edid_extra_block() {
        let header = repeat_n(0u8, 0x100);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x00, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(
            DrmConnector::parse_hdmi_edid_pa(&edid),
            Some(PhysicalAddress::from(0x1234))
        );
    }

    #[test]
    fn test_parse_edid_island() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 80
            0x65, 0x03, 0x0c, 0x00, 0x12, 0x34, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(
            DrmConnector::parse_hdmi_edid_pa(&edid),
            Some(PhysicalAddress::from(0x1234))
        );
    }

    #[test]
    fn test_parse_edid_skipped() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x00, 0x00, 0x6b, 0x00, 0x00, 0x00, // 80
            0x65, 0x03, 0x0c, 0x00, 0x12, 0x34, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_cutoff_0() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x04, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_cutoff_1() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x05, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_cutoff_2() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x06, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_cutoff_3() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x07, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_cutoff_4() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x08, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_cutoff_5() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x09, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_cutoff_6() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x0a, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(
            DrmConnector::parse_hdmi_edid_pa(&edid),
            Some(PhysicalAddress::from(0x1234))
        );
    }

    #[test]
    fn test_parse_edid_cutoff_7() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x0b, 0x00, 0x65, 0x03, 0x0c, 0x00, // 80
            0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(
            DrmConnector::parse_hdmi_edid_pa(&edid),
            Some(PhysicalAddress::from(0x1234))
        );
    }

    #[test]
    fn test_parse_edid_cutoff_end() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x7f, 0x00, 0x00, 0x00, 0x00, 0x00, // 80
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x65, 0x03, 0x0c, 0x00, 0x12, 0x34, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[test]
    fn test_parse_edid_cutoff_invalid_len() {
        let header = repeat_n(0u8, 0x80);
        let edid: [u8; 0x80] = [
            0x02, 0x03, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, // 80
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 88
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 90
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 98
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // A8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // B8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // C8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // D8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // E8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // F0
            0x00, 0x00, 0x65, 0x03, 0x0c, 0x00, 0x12, 0x34, // F8
        ];
        let edid: Vec<u8> = header.into_iter().chain(edid).collect();
        assert_eq!(DrmConnector::parse_hdmi_edid_pa(&edid), None);
    }

    #[tokio::test]
    async fn test_drm_connector_from_drm_path_invalid_base() {
        assert_eq!(
            format!(
                "{}",
                DrmConnector::from_drm_path("data/../data/test/card1-DP-1")
                    .await
                    .unwrap_err()
            ),
            "Not a DRM path"
        );
    }

    #[tokio::test]
    async fn test_drm_connector_from_drm_path_no_hyphen() {
        assert_eq!(
            format!(
                "{}",
                DrmConnector::from_drm_path("data/test/card1")
                    .await
                    .unwrap_err()
            ),
            "Invalid path"
        );
    }

    #[tokio::test]
    async fn test_drm_connector_from_drm_path_disconnected() {
        assert!(DrmConnector::from_drm_path("data/test/card1-DP-1")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_drm_connector_from_drm_path_no_edid() {
        assert!(DrmConnector::from_drm_path("data/test/card1-DP-2")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_drm_connector_from_drm_path_no_phys_addr() {
        assert!(DrmConnector::from_drm_path("data/test/card1-HDMI-A-1")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_drm_connector_from_drm_path_invalid_phys_addr() {
        assert!(DrmConnector::from_drm_path("data/test/card1-HDMI-A-2")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_drm_connector_from_drm_path_valid_phys_addr() {
        assert_eq!(
            DrmConnector::from_drm_path("data/test/card1-HDMI-B-1")
                .await
                .unwrap(),
            Some(DrmConnector {
                card: 1,
                connector: String::from("HDMI-B-1"),
                physical_address: PhysicalAddress::from(0x1234),
            })
        );
    }

    #[tokio::test]
    async fn test_drm_connector_from_drm_path_invalid_format() {
        assert!(DrmConnector::from_drm_path("data/test/cardZ-DP-1")
            .await
            .unwrap_err()
            .downcast::<ParseIntError>()
            .is_ok());
    }

    #[tokio::test]
    async fn test_connector_info_to_drm_connector_none() {
        assert_eq!(
            format!(
                "{}",
                DrmConnector::from_connector_info(
                    ConnectorInfo::None,
                    PhysicalAddress::from(0x1000)
                )
                .await
                .unwrap_err()
            ),
            "Not a DRM connector"
        );
    }

    #[tokio::test]
    async fn test_connector_info_to_drm_connector_unknown() {
        assert_eq!(
            format!(
                "{}",
                DrmConnector::from_connector_info(
                    ConnectorInfo::Unknown {
                        ty: 2,
                        data: [0; 16]
                    },
                    PhysicalAddress::from(0x1000)
                )
                .await
                .unwrap_err()
            ),
            "Not a DRM connector"
        );
    }

    #[tokio::test]
    async fn test_connector_info_to_drm_card_missing() {
        assert_eq!(
            DrmConnector::from_connector_info(
                ConnectorInfo::DrmConnector {
                    card_no: 0,
                    connector_id: 0,
                },
                PhysicalAddress::from(0x1000)
            )
            .await
            .unwrap_err()
            .downcast::<io::Error>()
            .unwrap()
            .kind(),
            ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn test_connector_info_to_drm_connector_missing() {
        assert_eq!(
            DrmConnector::from_connector_info(
                ConnectorInfo::DrmConnector {
                    card_no: 1,
                    connector_id: 0,
                },
                PhysicalAddress::from(0x1000)
            )
            .await
            .unwrap_err()
            .downcast::<io::Error>()
            .unwrap()
            .kind(),
            ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn test_connector_info_to_drm_connector_present() {
        assert_eq!(
            DrmConnector::from_connector_info(
                ConnectorInfo::DrmConnector {
                    card_no: 1,
                    connector_id: 2,
                },
                PhysicalAddress::from(0x1000)
            )
            .await
            .unwrap(),
            DrmConnector {
                card: 1,
                connector: String::from("DP-1"),
                physical_address: PhysicalAddress::from(0x1000),
            }
        );
    }

    #[tokio::test]
    async fn test_drm_connector_to_connector_info_missing_card() {
        assert_eq!(
            DrmConnector {
                card: 0,
                connector: String::from("DP-1"),
                physical_address: PhysicalAddress::default(),
            }
            .to_connector_info()
            .await
            .unwrap_err()
            .downcast::<io::Error>()
            .unwrap()
            .kind(),
            ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn test_drm_connector_to_connector_info_missing_connector() {
        assert_eq!(
            DrmConnector {
                card: 1,
                connector: String::from("DP-2"),
                physical_address: PhysicalAddress::default(),
            }
            .to_connector_info()
            .await
            .unwrap_err()
            .downcast::<io::Error>()
            .unwrap()
            .kind(),
            ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn test_drm_connector_to_connector_info_present() {
        assert_eq!(
            DrmConnector {
                card: 1,
                connector: String::from("DP-1"),
                physical_address: PhysicalAddress::default(),
            }
            .to_connector_info()
            .await
            .unwrap(),
            ConnectorInfo::DrmConnector {
                card_no: 1,
                connector_id: 2,
            }
        );
    }
}
