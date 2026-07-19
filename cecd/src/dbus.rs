/*
 * Copyright © 2024 Valve Software
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

use anyhow::{anyhow, bail};
#[cfg(not(test))]
use linux_cec::device::AsyncDevice;
use linux_cec::device::MessageData;
use linux_cec::message::{Message, Opcode};
use linux_cec::operand::{OperandEncodable, UiCommand};
use linux_cec::{
    Error as CecError, LogicalAddress, LogicalAddressType, PhysicalAddress, RxError, Timeout,
    TxError,
};
use num_enum::{TryFromPrimitive, TryFromPrimitiveError};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs::canonicalize;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::{spawn, JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zbus::message::Header;
use zbus::names::ErrorName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{fdo, interface, Connection, DBusError};

use crate::config::Config;
use crate::device::{DeviceTask, KeyRepeat};
use crate::system::{SystemHandle, SystemMessage};
use crate::uinput::UInputDevice;
use crate::ArcDevice;

#[cfg(test)]
use crate::testing::AsyncDevice;

#[allow(clippy::upper_case_acronyms)]
pub enum Error {
    Cec(CecError),
    FDO(fdo::Error),
    Anyhow(anyhow::Error),
}

impl DBusError for Error {
    fn create_reply(&self, header: &Header<'_>) -> zbus::Result<zbus::Message> {
        let builder = zbus::Message::error(header, self.name())?;
        match self {
            Error::Cec(e) => builder.build(&(format!("{e}"))),
            Error::FDO(e) => builder.build(&(format!("{e}"))),
            Error::Anyhow(e) => builder.build(&(format!("{e}"))),
        }
    }

    #[allow(deprecated)] // Required by DBusError's interface
    fn description(&self) -> Option<&str> {
        match self {
            Error::Cec(_) => None,
            Error::FDO(e) => <_ as DBusError>::description(e),
            Error::Anyhow(_) => None,
        }
    }

    fn name(&self) -> ErrorName<'_> {
        let err = match self {
            Error::Cec(e) => match e {
                CecError::OutOfRange { .. } => "com.steampowered.CecDaemon1.Error.OutOfRange",
                CecError::InvalidValueForType { .. } => {
                    "com.steampowered.CecDaemon1.Error.InvalidValueForType"
                }
                CecError::InvalidData => "com.steampowered.CecDaemon1.Error.InvalidData",
                CecError::Timeout => "com.steampowered.CecDaemon1.Error.Timeout",
                CecError::Abort => "com.steampowered.CecDaemon1.Error.Abort",
                CecError::NoLogicalAddress => "com.steampowered.CecDaemon1.Error.NoLogicalAddress",
                CecError::Disconnected => "com.steampowered.CecDaemon1.Error.Disconnected",
                CecError::Unsupported => "com.steampowered.CecDaemon1.Error.Unsupported",
                CecError::SystemError(_) => "com.steampowered.CecDaemon1.Error.SystemError",
                CecError::TxError(e) => match e {
                    TxError::ArbLost => "com.steampowered.CecDaemon1.TxError.ArbLost",
                    TxError::Nack => "com.steampowered.CecDaemon1.TxError.Nack",
                    TxError::LowDrive => "com.steampowered.CecDaemon1.TxError.LowDrive",
                    TxError::UnknownError => "com.steampowered.CecDaemon1.TxError.Unknown",
                    TxError::MaxRetries => "com.steampowered.CecDaemon1.TxError.MaxRetries",
                    TxError::Aborted => "com.steampowered.CecDaemon1.TxError.Aborted",
                    TxError::Timeout => "com.steampowered.CecDaemon1.TxError.Timeout",
                    _ => "com.steampowered.CecDaemon1.TxError.Unknown",
                },
                CecError::RxError(e) => match e {
                    RxError::Timeout => "com.steampowered.CecDaemon1.RxError.Timeout",
                    RxError::FeatureAbort => "com.steampowered.CecDaemon1.RxError.FeatureAbort",
                    RxError::Aborted => "com.steampowered.CecDaemon1.RxError.Aborted",
                    _ => "com.steampowered.CecDaemon1.RxError.Unknown",
                },
                _ => "com.steampowered.CecDaemon1.Error.Unknown",
            },
            Error::FDO(e) => return <_ as DBusError>::name(e),
            Error::Anyhow(_) => "org.freedesktop.DBus.Error.Failed",
        };
        ErrorName::from_str_unchecked(err)
    }
}

pub type Result<T> = core::result::Result<T, Error>;

impl From<CecError> for Error {
    fn from(err: CecError) -> Error {
        Error::Cec(err)
    }
}

impl From<fdo::Error> for Error {
    fn from(err: fdo::Error) -> Error {
        Error::FDO(err)
    }
}

impl From<zbus::Error> for Error {
    fn from(err: zbus::Error) -> Error {
        Error::FDO(err.into())
    }
}

impl From<anyhow::Error> for Error {
    fn from(err: anyhow::Error) -> Error {
        Error::Anyhow(err)
    }
}

impl<T: TryFromPrimitive> From<TryFromPrimitiveError<T>> for Error {
    fn from(err: TryFromPrimitiveError<T>) -> Error {
        Error::Cec(CecError::from(err))
    }
}

impl From<JoinError> for Error {
    fn from(err: JoinError) -> Error {
        Error::Anyhow(err.into())
    }
}

pub const PATH: &str = "/com/steampowered/CecDaemon1";

#[derive(Debug)]
pub struct CecConfig {
    system: SystemHandle,
    cached_config: Config,
}

impl CecConfig {
    pub async fn new(system: SystemHandle) -> CecConfig {
        let cached_config = system.lock().await.config.clone();
        CecConfig {
            system,
            cached_config,
        }
    }

    pub async fn reconfigure(&mut self, emitter: &SignalEmitter<'_>) {
        let mut new_config = self.system.lock().await.config.clone();
        if new_config.logical_address == LogicalAddressType::Unregistered {
            new_config.logical_address = LogicalAddressType::Playback;
        }
        let old_config = self.cached_config.clone();
        self.cached_config = new_config;

        if self.cached_config.osd_name != old_config.osd_name {
            if let Err(e) = self.osd_name_changed(emitter).await {
                warn!("Failed to emit OsdName changed: {e}");
            }
        }

        if self.cached_config.vendor_id != old_config.vendor_id {
            if let Err(e) = self.vendor_id_changed(emitter).await {
                warn!("Failed to emit VendorId changed: {e}");
            }
        }

        if self.cached_config.logical_address != old_config.logical_address {
            if let Err(e) = self.logical_address_changed(emitter).await {
                warn!("Failed to emit LogicalAddress changed: {e}");
            }
        }

        if self.cached_config.mappings != old_config.mappings {
            if let Err(e) = self.mappings_changed(emitter).await {
                warn!("Failed to emit Mappings changed: {e}");
            }
        }

        if self.cached_config.wake_tv != old_config.wake_tv {
            if let Err(e) = self.wake_tv_changed(emitter).await {
                warn!("Failed to emit WakeTv changed: {e}");
            }
        }

        if self.cached_config.suspend_tv != old_config.suspend_tv {
            if let Err(e) = self.suspend_tv_changed(emitter).await {
                warn!("Failed to emit SuspendTv changed: {e}");
            }
        }

        if self.cached_config.inactive_source_on_suspend != old_config.inactive_source_on_suspend {
            if let Err(e) = self.inactive_source_on_suspend_changed(emitter).await {
                warn!("Failed to emit InactiveSourceOnSuspend changed: {e}");
            }
        }

        if self.cached_config.allow_standby != old_config.allow_standby {
            if let Err(e) = self.allow_standby_changed(emitter).await {
                warn!("Failed to emit AllowStandby changed: {e}");
            }
        }

        if self.cached_config.uinput != old_config.uinput {
            if let Err(e) = self.uinput_changed(emitter).await {
                warn!("Failed to emit Uinput changed: {e}");
            }
        }

        if self.cached_config.request_active_source != old_config.request_active_source {
            if let Err(e) = self.request_active_source_changed(emitter).await {
                warn!("Failed to emit RequestActiveSource changed: {e}");
            }
        }
    }
}

#[interface(name = "com.steampowered.CecDaemon1.Config1")]
impl CecConfig {
    #[zbus(property)]
    pub async fn osd_name(&self) -> &str {
        self.cached_config.osd_name.as_deref().unwrap_or("")
    }

    #[zbus(property)]
    pub async fn vendor_id(&self) -> i32 {
        self.cached_config.vendor_id.map_or(-1, Into::<i32>::into)
    }

    #[zbus(property)]
    pub async fn logical_address(&self) -> u8 {
        self.cached_config.logical_address.into()
    }

    #[zbus(property)]
    pub async fn mappings(&self) -> Vec<(Vec<u8>, u16)> {
        self.cached_config
            .mappings
            .iter()
            .map(|(k, v)| {
                let mut kv = Vec::new();
                k.to_bytes(&mut kv);
                (kv, (*v).into())
            })
            .collect()
    }

    #[zbus(property)]
    pub async fn wake_tv(&self) -> bool {
        self.cached_config.wake_tv
    }

    #[zbus(property)]
    pub async fn suspend_tv(&self) -> bool {
        self.cached_config.suspend_tv
    }

    #[zbus(property)]
    pub async fn inactive_source_on_suspend(&self) -> bool {
        self.cached_config.inactive_source_on_suspend
    }

    #[zbus(property)]
    pub async fn allow_standby(&self) -> bool {
        self.cached_config.allow_standby
    }

    #[zbus(property)]
    pub async fn uinput(&self) -> bool {
        self.cached_config.uinput
    }

    #[zbus(property)]
    pub async fn request_active_source(&self) -> bool {
        self.cached_config.request_active_source
    }

    pub async fn reload(&self) -> Result<()> {
        Ok(self.system.reconfig().await?)
    }
}

#[derive(Debug)]
pub struct Daemon {
    system: SystemHandle,
}

impl Daemon {
    pub fn new(system: &SystemHandle) -> Daemon {
        Daemon {
            system: system.clone(),
        }
    }
}

#[interface(name = "com.steampowered.CecDaemon1.Daemon1")]
impl Daemon {
    async fn register_message_handler(
        &mut self,
        opcode: u8,
        path: ObjectPath<'_>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] signal_emitter: SignalEmitter<'_>,
    ) -> Result<bool> {
        let handled = self.system.list_handled_messages().await;
        if handled.contains(&opcode) {
            return Ok(false);
        }
        if !self
            .system
            .register_message_handler(
                opcode,
                path,
                header.sender().ok_or_else(|| {
                    fdo::Error::NameHasNoOwner(String::from(
                        "Cannot register message handler without a D-Bus unique name",
                    ))
                })?,
            )
            .await?
        {
            return Ok(false);
        }
        self.handled_messages_changed(&signal_emitter).await?;
        Ok(true)
    }

    async fn unregister_message_handler(
        &mut self,
        opcode: u8,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] signal_emitter: SignalEmitter<'_>,
    ) -> Result<bool> {
        if !self
            .system
            .unregister_message_handler(
                opcode,
                header.sender().ok_or_else(|| {
                    fdo::Error::NameHasNoOwner(String::from(
                        "Cannot register message handler without a D-Bus unique name",
                    ))
                })?,
            )
            .await?
        {
            return Ok(false);
        }
        self.handled_messages_changed(&signal_emitter).await?;
        Ok(true)
    }

    #[zbus(property)]
    async fn handled_messages(&self) -> Vec<u8> {
        let handled = self.system.list_handled_messages().await;
        let static_handled: HashSet<u8> = DeviceTask::STATIC_HANDLERS
            .iter()
            .map(|opcode| *opcode as u8)
            .collect();
        let mut handled: Vec<u8> = (&static_handled | &handled).into_iter().collect();
        handled.sort_unstable();
        handled
    }

    async fn wake(&self) {
        self.system.wake_all().await;
    }

    async fn standby(&self, force: bool) {
        self.system.standby_all(force).await;
    }
}

pub struct CecDevice {
    pub device: ArcDevice,
    pub token: CancellationToken,
    channel: Option<UnboundedSender<SystemMessage>>,
    path: PathBuf,
    dbus_path: OwnedObjectPath,
    pub cached_phys_addr: u16,
    pub cached_log_addrs: Vec<u8>,
    pub cached_vendor_id: i32,
    pub cached_active: bool,
    pub cached_audio_log_addr: u8,
    key_repeat: HashMap<u8, (UiCommand, CancellationToken, JoinHandle<anyhow::Result<()>>)>,
    pub uinput: Option<UInputDevice>,
}

impl CecDevice {
    const CURRENT_AUDIO_LOG_ADDR: u8 = 255;

    pub async fn open(
        path: impl AsRef<Path>,
        token: CancellationToken,
    ) -> anyhow::Result<CecDevice> {
        let path = canonicalize(path).await?;
        let device = AsyncDevice::open(&path).await?;

        let dbus_path = path.to_str().ok_or(anyhow!("Invalid path supplied"))?;
        let dbus_path = dbus_path.strip_prefix("/dev").unwrap_or(dbus_path);
        let dbus_path = dbus_path
            .split('/')
            .filter_map(|node| {
                // Capitalize the first letter of all path elements, if present
                let mut chars = node.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
            })
            .collect::<String>();
        let dbus_path = OwnedObjectPath::try_from(format!("{PATH}/Devices/{dbus_path}"))?;

        let cached_phys_addr = device
            .get_physical_address()
            .await
            .unwrap_or_default()
            .into();
        let cached_log_addrs = device
            .get_logical_addresses()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect();
        let cached_vendor_id = device
            .get_vendor_id()
            .await
            .unwrap_or_default()
            .map_or(-1, Into::<i32>::into);

        Ok(CecDevice {
            device: ArcDevice::from(device),
            token,
            path,
            dbus_path,
            channel: None,
            cached_phys_addr,
            cached_log_addrs,
            cached_vendor_id,
            cached_active: false,
            cached_audio_log_addr: LogicalAddress::Tv as u8,
            key_repeat: HashMap::new(),
            uinput: None,
        })
    }

    pub async fn register(
        self,
        connection: Connection,
        system: SystemHandle,
    ) -> anyhow::Result<()> {
        debug!("Registering CEC device {} on bus", self.path.display());

        let object_server = connection.object_server();
        let dbus_path = self.dbus_path.clone();
        object_server.at(dbus_path.as_ref(), self).await?;

        let interface = object_server.interface(dbus_path.as_ref()).await?;
        let broadcast_receiver = system.lock().await.subscribe();
        let (unicast_sender, unicast_receiver) = unbounded_channel();
        let task = match DeviceTask::new(
            interface.clone(),
            system,
            broadcast_receiver,
            unicast_receiver,
            connection.clone(),
        )
        .await
        {
            Ok(task) => task,
            Err(e) => {
                object_server
                    .remove::<CecDevice, _>(dbus_path.as_ref())
                    .await?;
                return Err(e);
            }
        };
        spawn(task.run());
        let mut interface = interface.get_mut().await;
        interface.channel = Some(unicast_sender);
        info!("Device {dbus_path} registered");
        Ok(())
    }

    pub fn dbus_path(&self) -> &ObjectPath<'_> {
        &self.dbus_path
    }

    pub async fn send_system_message(&self, message: SystemMessage) -> anyhow::Result<()> {
        let Some(ref tx) = self.channel else {
            bail!("Device task has not started");
        };
        tx.send(message)?;
        Ok(())
    }
}

#[interface(name = "com.steampowered.CecDaemon1.CecDevice1")]
impl CecDevice {
    #[zbus(signal)]
    async fn received_message(
        signal_emitter: &SignalEmitter<'_>,
        initiator: u8,
        destination: u8,
        timestamp: u64,
        message: &[u8],
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn user_control_pressed(
        signal_emitter: &SignalEmitter<'_>,
        button: &[u8],
        initiator: u8,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn user_control_released(
        signal_emitter: &SignalEmitter<'_>,
        initiator: u8,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    async fn logical_addresses(&self) -> Vec<u8> {
        self.cached_log_addrs.clone()
    }

    #[zbus(property)]
    async fn physical_address(&self) -> u16 {
        self.cached_phys_addr
    }

    #[zbus(property)]
    async fn vendor_id(&self) -> i32 {
        self.cached_vendor_id
    }

    #[zbus(property)]
    async fn active(&self) -> bool {
        self.cached_active
    }

    async fn set_osd_name(&self, name: &str) -> Result<()> {
        Ok(self.device.lock().await.set_osd_name(name).await?)
    }

    async fn set_active_source(&self, phys_addr: i32) -> Result<()> {
        let phys_addr = match <_ as TryInto<u16>>::try_into(phys_addr) {
            Ok(phys_addr) => Some(PhysicalAddress::from(phys_addr)),
            Err(_) => None,
        };
        Ok(self
            .device
            .lock()
            .await
            .set_active_source(phys_addr)
            .await?)
    }

    async fn wake(&self) -> Result<()> {
        Ok(self
            .send_system_message(SystemMessage::Wake {
                wake_tv: true,
                from_standby: false,
            })
            .await?)
    }

    async fn standby(&self, target: u8) -> Result<()> {
        let target = LogicalAddress::try_from_primitive(target)?;
        Ok(self.device.lock().await.standby(target).await?)
    }

    async fn press_user_control(&mut self, button: &[u8], target: u8) -> Result<()> {
        let log_addr = LogicalAddress::try_from_primitive(target)?;
        let key = UiCommand::try_from_bytes(button)?;
        if let Some((current_key, token, _)) = self.key_repeat.get(&target) {
            if &key == current_key {
                return Ok(());
            }
            token.cancel();
            let (_, _, handle) = self.key_repeat.remove(&target).unwrap();
            handle.await??;
        }
        let token = self.token.child_token();
        let task = KeyRepeat {
            device: self.device.clone(),
            token: token.clone(),
            log_addr,
            key,
        };
        let handle = spawn(task.run());
        self.key_repeat.insert(target, (key, token, handle));
        Ok(())
    }

    async fn release_user_control(&mut self, target: u8) -> Result<()> {
        if let Some((_, token, handle)) = self.key_repeat.remove(&target) {
            token.cancel();
            handle.await??;
            Ok(())
        } else {
            let target = LogicalAddress::try_from_primitive(target)?;
            Ok(self
                .device
                .lock()
                .await
                .release_user_control(target)
                .await?)
        }
    }

    async fn press_once_user_control(&mut self, button: &[u8], target: u8) -> Result<()> {
        let log_addr = LogicalAddress::try_from_primitive(target)?;
        let key = UiCommand::try_from_bytes(button)?;
        if let Some((_, token, _)) = self.key_repeat.get(&target) {
            token.cancel();
            let (current_key, _, handle) = self.key_repeat.remove(&target).unwrap();
            handle.await??;
            if key == current_key {
                return Ok(());
            }
        }
        let device = self.device.lock().await;
        device.press_user_control(key, log_addr).await?;
        device.release_user_control(log_addr).await?;
        Ok(())
    }

    async fn volume_up(&self, target: u8) -> Result<()> {
        let target =
            LogicalAddress::try_from_primitive(if target == CecDevice::CURRENT_AUDIO_LOG_ADDR {
                self.cached_audio_log_addr
            } else {
                target
            })?;
        let device = self.device.lock().await;
        device
            .press_user_control(UiCommand::VolumeUp, target)
            .await?;
        device.release_user_control(target).await?;
        Ok(())
    }

    async fn volume_down(&self, target: u8) -> Result<()> {
        let target =
            LogicalAddress::try_from_primitive(if target == CecDevice::CURRENT_AUDIO_LOG_ADDR {
                self.cached_audio_log_addr
            } else {
                target
            })?;
        let device = self.device.lock().await;
        device
            .press_user_control(UiCommand::VolumeDown, target)
            .await?;
        device.release_user_control(target).await?;
        Ok(())
    }

    async fn mute(&self, target: u8) -> Result<()> {
        let target =
            LogicalAddress::try_from_primitive(if target == CecDevice::CURRENT_AUDIO_LOG_ADDR {
                self.cached_audio_log_addr
            } else {
                target
            })?;
        let device = self.device.lock().await;
        device.press_user_control(UiCommand::Mute, target).await?;
        device.release_user_control(target).await?;
        Ok(())
    }

    async fn get_audio_status(&self, target: u8) -> Result<(u8, bool)> {
        let target =
            LogicalAddress::try_from_primitive(if target == CecDevice::CURRENT_AUDIO_LOG_ADDR {
                self.cached_audio_log_addr
            } else {
                target
            })?;
        let reply = self
            .device
            .lock()
            .await
            .tx_rx_message(
                &Message::GiveAudioStatus,
                target,
                Opcode::ReportAudioStatus,
                Timeout::MAX,
            )
            .await?;
        let MessageData::Valid(Message::ReportAudioStatus { status }) = reply.message else {
            return Err(Error::Cec(CecError::InvalidData));
        };
        Ok((status.volume().try_into().unwrap(), status.mute()))
    }

    #[zbus(property)]
    async fn audio_logical_address(&self) -> u8 {
        self.cached_audio_log_addr
    }

    async fn send_raw_message(&self, raw_message: &[u8], target: u8) -> Result<u32> {
        let target = LogicalAddress::try_from_primitive(target)?;
        let raw_message = Message::try_from_bytes(raw_message)?;
        Ok(self
            .device
            .lock()
            .await
            .tx_message(&raw_message, target)
            .await?)
    }

    async fn send_receive_raw_message(
        &self,
        raw_message: &[u8],
        target: u8,
        opcode: u8,
        timeout: u16,
    ) -> Result<Vec<u8>> {
        let target = LogicalAddress::try_from_primitive(target)?;
        let raw_message = Message::try_from_bytes(raw_message)?;
        let reply = self
            .device
            .lock()
            .await
            .tx_rx_message(
                &raw_message,
                target,
                Opcode::try_from_primitive(opcode)?,
                Timeout::from_ms(timeout.into()),
            )
            .await?;
        Ok(reply.message.to_bytes())
    }

    async fn poll(&self, target: u8) -> Result<()> {
        let target = LogicalAddress::try_from_primitive(target)?;
        Ok(self.device.lock().await.poll_address(target).await?)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::system::System;
    use crate::testing::{setup_basic_test, setup_dbus_test, DBusTest};
    use cecd_proxy::Config1Proxy;
    use input_linux::Key;
    use linux_cec::device::Capabilities;
    use linux_cec::VendorId;
    use nix::unistd::gethostname;
    use tokio_stream::StreamExt;

    async fn setup_config_test(
        config: &Config,
    ) -> anyhow::Result<(DBusTest<'static>, Config1Proxy<'static>)> {
        async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
            let mut dev = dev.lock().await;
            dev.set_caps(Capabilities::LOG_ADDRS | Capabilities::TRANSMIT);
            dev.set_phys_addr(PhysicalAddress::from(0x1000)).await;
            Ok(())
        }
        let test = setup_dbus_test(cb, Some(config.clone())).await?;

        let config_obj = CecConfig::new(test.system.clone()).await;
        test.connection
            .object_server()
            .at(format!("{PATH}/Daemon"), config_obj)
            .await?;

        let config_proxy = Config1Proxy::builder(&test.connection).build().await?;

        Ok((test, config_proxy))
    }

    #[tokio::test]
    async fn test_default_config_readout() {
        let config = Config::default();
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(
            config_proxy.osd_name().await.unwrap(),
            config
                .osd_name
                .unwrap_or_else(|| gethostname().unwrap().into_string().unwrap())
        );
        assert_eq!(
            config_proxy.vendor_id().await.unwrap(),
            config.vendor_id.map(Into::<i32>::into).unwrap_or(-1)
        );
        assert_eq!(
            config_proxy.logical_address().await.unwrap(),
            (if config.logical_address == LogicalAddressType::Unregistered {
                LogicalAddressType::Playback
            } else {
                config.logical_address
            })
            .into()
        );
        assert_eq!(
            HashMap::from_iter(config_proxy.mappings().await.unwrap()),
            (if !config.mappings.is_empty() {
                config
                    .mappings
                    .iter()
                    .map(|(k, v)| {
                        let mut kv = Vec::new();
                        k.to_bytes(&mut kv);
                        (kv, (*v).into())
                    })
                    .collect::<HashMap<_, _>>()
            } else {
                System::DEFAULT_MAPPINGS
                    .iter()
                    .map(|(k, v)| {
                        let mut kv = Vec::new();
                        k.to_bytes(&mut kv);
                        (kv, (*v).into())
                    })
                    .collect::<HashMap<_, _>>()
            })
        );
        assert_eq!(config_proxy.wake_tv().await.unwrap(), config.wake_tv);
        assert_eq!(config_proxy.suspend_tv().await.unwrap(), config.suspend_tv);
        assert_eq!(
            config_proxy.inactive_source_on_suspend().await.unwrap(),
            config.inactive_source_on_suspend
        );
        assert_eq!(
            config_proxy.allow_standby().await.unwrap(),
            config.allow_standby
        );
        assert_eq!(config_proxy.uinput().await.unwrap(), config.uinput);
    }

    #[tokio::test]
    async fn test_osd_name_config_readout() {
        let config = Config {
            osd_name: Some(String::from("CEC2")),
            ..Config::default()
        };
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(
            config_proxy.osd_name().await.unwrap(),
            config.osd_name.unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn test_osd_name_config_reconfig() {
        let config = Config {
            osd_name: Some(String::from("CEC2")),
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy.receive_osd_name_changed().await;
        test.system.lock().await.config.osd_name = Some(String::from("CEC3"));
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert_eq!(msg.get().await.unwrap(), "CEC3");
    }

    #[tokio::test]
    async fn test_vendor_id_config_readout() {
        let config = Config {
            vendor_id: Some(VendorId([0x12, 0x34, 0x56])),
            ..Config::default()
        };
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(
            config_proxy.vendor_id().await.unwrap(),
            config.vendor_id.map(Into::<i32>::into).unwrap_or(-1)
        );
    }

    #[tokio::test]
    async fn test_vendor_id_config_reconfig() {
        let config = Config {
            vendor_id: Some(VendorId([0x12, 0x34, 0x56])),
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy.receive_vendor_id_changed().await;
        test.system.lock().await.config.vendor_id = Some(VendorId([0x56, 0x34, 0x12]));
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert_eq!(
            msg.get().await.unwrap(),
            VendorId([0x56, 0x34, 0x12]).into()
        );
    }

    #[tokio::test]
    async fn test_logical_address_config_readout() {
        let config = Config {
            logical_address: LogicalAddressType::AudioSystem,
            ..Config::default()
        };
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(
            config_proxy.logical_address().await.unwrap(),
            config.logical_address.into()
        );
    }

    #[tokio::test]
    async fn test_logical_address_config_reconfig() {
        let config = Config {
            logical_address: LogicalAddressType::AudioSystem,
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy.receive_logical_address_changed().await;
        test.system.lock().await.config.logical_address = LogicalAddressType::Record;
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert_eq!(msg.get().await.unwrap(), LogicalAddressType::Record.into());
    }

    #[tokio::test]
    async fn test_mappings_config_readout() {
        let config = Config {
            mappings: [(UiCommand::Enter, Key::Enter)].into(),
            ..Config::default()
        };
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(
            HashMap::from_iter(config_proxy.mappings().await.unwrap()),
            config
                .mappings
                .iter()
                .map(|(k, v)| {
                    let mut kv = Vec::new();
                    k.to_bytes(&mut kv);
                    (kv, (*v).into())
                })
                .collect::<HashMap<_, _>>()
        );
    }

    #[tokio::test]
    async fn test_wake_tv_config_readout() {
        let mut config = Config::default();
        config.wake_tv = !config.wake_tv;
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(config_proxy.wake_tv().await.unwrap(), config.wake_tv);
    }

    #[tokio::test]
    async fn test_wake_tv_config_reconfig() {
        let config = Config {
            wake_tv: false,
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy.receive_wake_tv_changed().await;
        test.system.lock().await.config.wake_tv = true;
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert!(msg.get().await.unwrap());
    }

    #[tokio::test]
    async fn test_suspend_tv_config_readout() {
        let mut config = Config::default();
        config.suspend_tv = !config.suspend_tv;
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(config_proxy.suspend_tv().await.unwrap(), config.suspend_tv);
    }

    #[tokio::test]
    async fn test_suspend_tv_config_reconfig() {
        let config = Config {
            suspend_tv: false,
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy.receive_suspend_tv_changed().await;
        test.system.lock().await.config.suspend_tv = true;
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert!(msg.get().await.unwrap());
    }

    #[tokio::test]
    async fn test_inactive_source_on_suspend_config_readout() {
        let mut config = Config::default();
        config.inactive_source_on_suspend = !config.inactive_source_on_suspend;
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(
            config_proxy.inactive_source_on_suspend().await.unwrap(),
            config.inactive_source_on_suspend
        );
    }

    #[tokio::test]
    async fn test_inactive_source_on_suspend_config_reconfig() {
        let config = Config {
            inactive_source_on_suspend: false,
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy
            .receive_inactive_source_on_suspend_changed()
            .await;
        test.system.lock().await.config.inactive_source_on_suspend = true;
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert!(msg.get().await.unwrap());
    }

    #[tokio::test]
    async fn test_allow_standby_config_readout() {
        let mut config = Config::default();
        config.allow_standby = !config.allow_standby;
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(
            config_proxy.allow_standby().await.unwrap(),
            config.allow_standby
        );
    }

    #[tokio::test]
    async fn test_allow_standby_config_reconfig() {
        let config = Config {
            allow_standby: false,
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy.receive_allow_standby_changed().await;
        test.system.lock().await.config.allow_standby = true;
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert!(msg.get().await.unwrap());
    }

    #[tokio::test]
    async fn test_uinput_config_readout() {
        let mut config = Config::default();
        config.uinput = !config.uinput;
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(config_proxy.uinput().await.unwrap(), config.uinput);
    }

    #[tokio::test]
    async fn test_uinput_config_reconfig() {
        let config = Config {
            uinput: false,
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy.receive_uinput_changed().await;
        test.system.lock().await.config.uinput = true;
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert!(msg.get().await.unwrap());
    }

    #[tokio::test]
    async fn test_request_active_source_config_readout() {
        let mut config = Config::default();
        config.request_active_source = !config.request_active_source;
        let (_test, config_proxy) = setup_config_test(&config).await.unwrap();

        assert_eq!(
            config_proxy.request_active_source().await.unwrap(),
            config.request_active_source
        );
    }

    #[tokio::test]
    async fn test_request_active_source_config_reconfig() {
        let config = Config {
            request_active_source: false,
            ..Config::default()
        };
        let (test, config_proxy) = setup_config_test(&config).await.unwrap();

        let mut receiver = config_proxy.receive_request_active_source_changed().await;
        test.system.lock().await.config.request_active_source = true;
        let iface = test
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await
            .unwrap();
        iface
            .get_mut()
            .await
            .reconfigure(iface.signal_emitter())
            .await;
        let msg = receiver.next().await.unwrap();
        assert!(msg.get().await.unwrap());
    }

    #[tokio::test]
    async fn test_volume_up() {
        let test = setup_basic_test().await.unwrap();
        test.proxy
            .volume_up(LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.dev.lock().await.key_repeat.notify_one();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::VolumeUp
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
    async fn test_volume_down() {
        let test = setup_basic_test().await.unwrap();
        test.proxy
            .volume_down(LogicalAddress::Tv.into())
            .await
            .unwrap();
        test.dev.lock().await.key_repeat.notify_one();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::VolumeDown
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
    async fn test_mute() {
        let test = setup_basic_test().await.unwrap();
        test.proxy.mute(LogicalAddress::Tv.into()).await.unwrap();
        test.dev.lock().await.key_repeat.notify_one();

        assert_eq!(
            test.dev.lock().await.dequeue_tx_message().await,
            Some((
                Message::UserControlPressed {
                    ui_command: UiCommand::Mute
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
}
