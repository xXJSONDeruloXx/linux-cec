/*
 * Copyright © 2024 Valve Software
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

use anyhow::bail;
use linux_cec::device::{
    Capabilities, ConnectorInfo, Envelope, MessageData, PollResult, PollStatus, PollTimeout,
};
use linux_cec::message::{Message, Opcode};
use linux_cec::operand::{BufferOperand, UiCommand};
use linux_cec::{
    Error, FollowerMode, InitiatorMode, LogicalAddress, LogicalAddressType, PhysicalAddress,
    Result, Timeout, VendorId,
};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::mem::drop;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::dispatcher::DefaultGuard;
use tracing::{debug, subscriber};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};
use zbus::connection::Builder;
use zbus::names::OwnedErrorName;
use zbus::{proxy, Connection};

pub(crate) mod dbus;

use crate::config::Config;
use crate::system::{System, SystemHandle};
pub(crate) use crate::testing::dbus::MockDBus;
use crate::ArcDevice;

#[derive(Clone, Debug)]
struct DeviceState {
    pollers: Vec<Sender<PollStatus>>,
    follower: FollowerMode,
    initiator: InitiatorMode,
    phys_addr: PhysicalAddress,
    log_addrs: Vec<LogicalAddress>,
    osd_name: BufferOperand,
    vendor_id: Option<VendorId>,
    tx_queue: VecDeque<(Message, LogicalAddress)>,
    rx_queue: VecDeque<Envelope>,
    rx_empty: Vec<Arc<Notify>>,
    sequence: u32,
}

#[derive(Debug)]
pub struct AsyncDevice {
    _path: PathBuf,
    caps: Capabilities,
    token: CancellationToken,
    force_unregistered: bool,
    driver_name: OsString,
    adapter_name: OsString,
    state: RwLock<DeviceState>,
    pub key_repeat: Arc<Notify>,
}

#[derive(Debug)]
pub struct AsyncDevicePoller {
    token: CancellationToken,
    channel: UnsafeCell<Receiver<PollStatus>>,
}

unsafe impl Sync for AsyncDevicePoller {}

impl AsyncDevice {
    pub async fn open(path: impl AsRef<Path>) -> Result<AsyncDevice> {
        Ok(AsyncDevice {
            _path: path.as_ref().to_path_buf(),
            caps: Capabilities::empty(),
            token: CancellationToken::new(),
            force_unregistered: false,
            key_repeat: Arc::new(Notify::new()),
            driver_name: OsString::new(),
            adapter_name: OsString::new(),
            state: RwLock::new(DeviceState {
                pollers: Vec::new(),
                follower: FollowerMode::Disabled,
                initiator: InitiatorMode::Disabled,
                phys_addr: PhysicalAddress::default(),
                log_addrs: Vec::new(),
                osd_name: BufferOperand::default(),
                vendor_id: None,
                tx_queue: VecDeque::new(),
                rx_queue: VecDeque::new(),
                rx_empty: Vec::new(),
                sequence: 1,
            }),
        })
    }

    pub(crate) fn set_caps(&mut self, caps: Capabilities) {
        self.caps = caps;
    }

    pub(crate) async fn set_phys_addr(&mut self, phys_addr: PhysicalAddress) {
        self.state.write().await.phys_addr = phys_addr;
    }

    pub(crate) async fn queue_rx_message(&self, message: Message, destination: LogicalAddress) {
        let mut state = self.state.write().await;
        let envelope = Envelope {
            message: MessageData::Valid(message),
            initiator: state.log_addrs[0],
            destination,
            timestamp: 0,
            sequence: state.sequence,
        };
        state.sequence += 1;
        state.rx_queue.push_back(envelope);
        for poller in state.pollers.iter() {
            poller.send(PollStatus::GotMessage).await.unwrap();
        }
    }

    pub(crate) async fn send_rx_message(
        &self,
        message: Message,
        initiator: LogicalAddress,
    ) -> Arc<Notify> {
        let mut state = self.state.write().await;
        let envelope = Envelope {
            message: MessageData::Valid(message),
            initiator,
            destination: state.log_addrs[0],
            timestamp: 0,
            sequence: state.sequence,
        };
        state.sequence += 1;
        state.rx_queue.push_back(envelope);
        let notify = Arc::new(Notify::new());
        state.rx_empty.push(notify.clone());
        for poller in state.pollers.iter() {
            poller.send(PollStatus::GotMessage).await.unwrap();
        }
        notify
    }

    pub(crate) async fn rx_queue_empty(&mut self) -> Option<Arc<Notify>> {
        let mut state = self.state.write().await;
        if state.rx_queue.is_empty() {
            return None;
        }
        let notify = Arc::new(Notify::new());
        state.rx_empty.push(notify.clone());
        Some(notify)
    }

    pub(crate) async fn dequeue_tx_message(&self) -> Option<(Message, LogicalAddress)> {
        self.state.write().await.tx_queue.pop_front()
    }

    pub async fn set_blocking(&self, _blocking: bool) -> Result<()> {
        todo!();
    }

    pub async fn get_poller(&self) -> Result<AsyncDevicePoller> {
        let (tx, rx) = channel(8);
        self.state.write().await.pollers.push(tx);
        Ok(AsyncDevicePoller {
            channel: UnsafeCell::new(rx),
            token: self.token.clone(),
        })
    }

    pub async fn poll(&self, _timeout: PollTimeout) -> Result<Vec<PollResult>> {
        todo!();
    }

    pub async fn set_initiator_mode(&self, mode: InitiatorMode) -> Result<()> {
        if !self.caps.contains(Capabilities::TRANSMIT) {
            return Err(Error::InvalidData);
        }
        self.state.write().await.initiator = mode;
        Ok(())
    }

    pub async fn set_follower_mode(&self, mode: FollowerMode) -> Result<()> {
        self.state.write().await.follower = mode;
        Ok(())
    }

    pub async fn get_capabilities(&self) -> Result<Capabilities> {
        Ok(self.caps)
    }

    pub async fn get_driver_name(&self) -> Result<OsString> {
        Ok(self.driver_name.clone())
    }

    pub async fn get_adapter_name(&self) -> Result<OsString> {
        Ok(self.adapter_name.clone())
    }

    pub async fn get_physical_address(&self) -> Result<PhysicalAddress> {
        Ok(self.state.read().await.phys_addr)
    }

    pub async fn set_physical_address(&self, phys_addr: PhysicalAddress) -> Result<()> {
        if !self.caps.contains(Capabilities::PHYS_ADDR) {
            return Err(Error::InvalidData);
        }
        self.state.write().await.phys_addr = phys_addr;
        Ok(())
    }

    pub async fn get_logical_addresses(&self) -> Result<Vec<LogicalAddress>> {
        Ok(self.state.read().await.log_addrs.clone())
    }

    pub async fn set_logical_addresses(&self, log_addrs: &[LogicalAddressType]) -> Result<()> {
        if !self.caps.contains(Capabilities::LOG_ADDRS) {
            return Err(Error::InvalidData);
        }
        {
            let state = self.state.read().await;
            if state.initiator == InitiatorMode::Disabled {
                return Err(Error::InvalidData);
            }
            if !state.log_addrs.is_empty() {
                return Err(Error::InvalidData);
            }
            if !state.phys_addr.is_valid() {
                return Err(Error::InvalidData);
            }
        }
        let mut state = self.state.write().await;
        if self.force_unregistered {
            state.log_addrs = vec![LogicalAddress::Unregistered];
        } else {
            state.log_addrs = log_addrs
                .iter()
                .map(|ty| match *ty {
                    LogicalAddressType::Tv => LogicalAddress::Tv,
                    LogicalAddressType::Record => LogicalAddress::RecordingDevice1,
                    LogicalAddressType::Tuner => LogicalAddress::Tuner1,
                    LogicalAddressType::Playback => LogicalAddress::PlaybackDevice1,
                    LogicalAddressType::AudioSystem => LogicalAddress::AudioSystem,
                    LogicalAddressType::Specific => LogicalAddress::Specific,
                    LogicalAddressType::Unregistered => LogicalAddress::Unregistered,
                })
                .collect();
        }
        for poller in state.pollers.iter() {
            poller.send(PollStatus::GotEvent).await.unwrap();
        }
        Ok(())
    }

    pub async fn set_logical_address(&self, log_addr: LogicalAddressType) -> Result<()> {
        self.set_logical_addresses(&[log_addr]).await
    }

    pub async fn clear_logical_addresses(&self) -> Result<()> {
        if !self.caps.contains(Capabilities::LOG_ADDRS) {
            return Err(Error::InvalidData);
        }
        let mut state = self.state.write().await;
        state.log_addrs.clear();
        for poller in state.pollers.iter() {
            poller.send(PollStatus::GotEvent).await.unwrap();
        }
        Ok(())
    }

    pub async fn get_osd_name(&self) -> Result<OsString> {
        Ok(OsStr::from_bytes(self.state.read().await.osd_name.as_bytes()).to_os_string())
    }

    pub async fn set_osd_name(&self, name: &str) -> Result<()> {
        self.state.write().await.osd_name = BufferOperand::from_str(name)?;
        Ok(())
    }

    pub async fn get_vendor_id(&self) -> Result<Option<VendorId>> {
        Ok(self.state.read().await.vendor_id)
    }

    pub async fn set_vendor_id(&self, vendor_id: Option<VendorId>) -> Result<()> {
        self.state.write().await.vendor_id = vendor_id;
        Ok(())
    }

    pub async fn tx_message(&self, message: &Message, destination: LogicalAddress) -> Result<u32> {
        let mut state = self.state.write().await;
        if state.log_addrs.is_empty() {
            return Err(Error::NoLogicalAddress);
        }
        if !state
            .log_addrs
            .iter()
            .any(|addr| *addr != LogicalAddress::Unregistered)
        {
            return Err(Error::InvalidData);
        }
        if state.initiator == InitiatorMode::Disabled {
            return Err(Error::InvalidData);
        }
        state.tx_queue.push_back((*message, destination));
        let seq = state.sequence;
        state.sequence += 1;
        Ok(seq)
    }

    pub async fn tx_rx_message(
        &self,
        _message: &Message,
        _destination: LogicalAddress,
        _opcode: Opcode,
        _timeout: Timeout,
    ) -> Result<Envelope> {
        todo!();
    }

    pub async fn rx_message(&self, _timeout: Timeout) -> Result<Envelope> {
        let mut state = self.state.write().await;
        if state.log_addrs.is_empty() {
            return Err(Error::NoLogicalAddress);
        }
        if state.follower == FollowerMode::Disabled {
            return Err(Error::InvalidData);
        }
        if let Some(message) = state.rx_queue.pop_front() {
            Ok(message)
        } else {
            Err(Error::Timeout)
        }
    }

    pub async fn poll_address(&self, _destination: LogicalAddress) -> Result<()> {
        Ok(())
    }

    pub async fn handle_status(&self, status: PollStatus) -> Result<Vec<PollResult>> {
        let mut results = Vec::new();
        if status.got_event() {
            results.push(PollResult::StateChange);
        }

        if status.got_message() {
            let mut state = self.state.write().await;
            if let Some(message) = state.rx_queue.pop_front() {
                if state.rx_queue.is_empty() {
                    for notify in state.rx_empty.drain(..) {
                        notify.notify_one();
                    }
                }
                drop(state);
                results.push(PollResult::Message(message));
            }
        }
        Ok(results)
    }

    pub async fn get_connector_info(&self) -> Result<ConnectorInfo> {
        Ok(ConnectorInfo::None)
    }

    pub async fn set_active_source(&self, _address: Option<PhysicalAddress>) -> Result<()> {
        todo!();
    }

    pub async fn wake(&self, _set_active: bool, _text_view: bool) -> Result<()> {
        todo!();
    }

    pub async fn standby(&self, target: LogicalAddress) -> Result<()> {
        let standby = Message::Standby {};
        self.tx_message(&standby, target).await?;
        Ok(())
    }

    pub async fn press_user_control(
        &self,
        ui_command: UiCommand,
        target: LogicalAddress,
    ) -> Result<()> {
        let user_control = Message::UserControlPressed { ui_command };
        self.tx_message(&user_control, target).await?;
        Ok(())
    }

    pub async fn release_user_control(&self, target: LogicalAddress) -> Result<()> {
        let user_control = Message::UserControlReleased {};
        self.tx_message(&user_control, target).await?;
        Ok(())
    }

    pub async fn close(self) -> Result<()> {
        self.token.cancel();
        Ok(())
    }
}

impl AsyncDevicePoller {
    pub async fn poll(&self, _timeout: PollTimeout) -> Result<PollStatus> {
        let channel = unsafe { &mut *self.channel.get() };
        Ok(select! {
            _ = self.token.cancelled() => PollStatus::Destroyed,
            status = channel.recv() => if let Some(status) = status {
                debug!("Got message {status:?}");
                status
            } else {
                PollStatus::Destroyed
            }
        })
    }
}

#[proxy(
    interface = "com.steampowered.CecDaemon1.CecDevice1",
    default_service = "com.steampowered.CecDaemon1"
)]
pub(crate) trait CecDevice {
    #[zbus(signal)]
    fn received_message(
        initiator: u8,
        destination: u8,
        timestamp: u64,
        message: &[u8],
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn user_control_pressed(button: &[u8], initiator: u8) -> zbus::Result<()>;

    #[zbus(signal)]
    fn user_control_released(initiator: u8) -> zbus::Result<()>;

    #[zbus(property)]
    fn logical_addresses(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property)]
    fn physical_address(&self) -> zbus::Result<u16>;

    #[zbus(property)]
    fn vendor_id(&self) -> zbus::Result<i32>;

    fn set_osd_name(&self, name: &str) -> zbus::Result<()>;

    fn set_active_source(&self, phys_addr: i32) -> zbus::Result<()>;

    fn wake(&self) -> zbus::Result<()>;

    fn standby(&self, target: u8) -> zbus::Result<()>;

    fn press_user_control(&mut self, button: &[u8], target: u8) -> zbus::Result<()>;

    fn release_user_control(&mut self, target: u8) -> zbus::Result<()>;

    fn press_once_user_control(&mut self, button: &[u8], target: u8) -> zbus::Result<()>;

    fn volume_up(&self, target: u8) -> zbus::Result<()>;

    fn volume_down(&self, target: u8) -> zbus::Result<()>;

    fn mute(&self, target: u8) -> zbus::Result<()>;

    fn send_raw_message(&self, raw_message: &[u8], target: u8) -> zbus::Result<u32>;

    fn send_receive_raw_message(
        &self,
        raw_message: &[u8],
        target: u8,
        opcode: u8,
        timeout: u16,
    ) -> zbus::Result<Vec<u8>>;
}

pub(crate) struct DBusTest<'a> {
    pub dev: ArcDevice,
    pub proxy: CecDeviceProxy<'a>,
    pub connection: Connection,
    pub system: SystemHandle,
    pub dbus: MockDBus,
    _guard: DefaultGuard,
}

pub(crate) async fn setup_dbus_test<F, Fut>(
    setup_dev: F,
    config: Option<Config>,
) -> anyhow::Result<DBusTest<'static>>
where
    F: FnOnce(ArcDevice) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let guard = subscriber::set_default(
        tracing_subscriber::registry()
            .with(fmt::layer())
            .with(EnvFilter::from_default_env()),
    );

    debug!("Setting up DBus test");
    let dbus = MockDBus::new().await?;
    let daemon_conn = Builder::address(dbus.address())?
        .name("com.steampowered.CecDaemon1")?
        .build()
        .await?;
    let client_conn = dbus.new_connection().await?;
    debug!("Got DBus connection");

    let token = CancellationToken::new();
    let system = SystemHandle(Arc::new(Mutex::new(
        System::new(token.clone(), daemon_conn, client_conn, None).await?,
    )));
    let config = config.unwrap_or_else(|| Config {
        uinput: false,
        ..Config::default()
    });
    system.lock().await.set_config(config).await?;
    debug!("System created");

    let dev;
    let connection;
    {
        let mut system = system.lock().await;
        dev = system.find_dev("/dev/null").await?;
        connection = system.connection.clone();
    }
    debug!("Device created");
    let arc_dev = dev.device.clone();
    setup_dev(arc_dev.clone()).await?;
    dev.register(connection.clone(), system.clone()).await?;

    let log_addr = arc_dev
        .lock()
        .await
        .get_logical_addresses()
        .await
        .unwrap_or_default()
        .first()
        .copied();
    if !matches!(log_addr, None | Some(LogicalAddress::Unregistered)) {
        while arc_dev.lock().await.dequeue_tx_message().await.is_none() {
            sleep(Duration::from_millis(1)).await;
        }
        while arc_dev.lock().await.dequeue_tx_message().await.is_some() {}
    }
    debug!("Device registered");

    Ok(DBusTest {
        dev: arc_dev,
        proxy: CecDeviceProxy::new(&connection, "/com/steampowered/CecDaemon1/Devices/Null")
            .await?,
        connection,
        system,
        dbus,
        _guard: guard,
    })
}

pub(crate) async fn rx_message(dev: &ArcDevice) -> Option<(Message, LogicalAddress)> {
    for _ in 0..100 {
        let Some(message) = dev.lock().await.dequeue_tx_message().await else {
            sleep(Duration::from_millis(1)).await;
            continue;
        };
        return Some(message);
    }
    None
}

pub(crate) async fn tx_message(dev: &ArcDevice, message: Message, initiator: LogicalAddress) {
    let notify = dev.lock().await.send_rx_message(message, initiator).await;
    notify.notified().await;
}

pub(crate) async fn setup_basic_test() -> anyhow::Result<DBusTest<'static>> {
    async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
        let mut dev = dev.lock().await;
        dev.set_caps(Capabilities::LOG_ADDRS | Capabilities::TRANSMIT);
        dev.set_phys_addr(PhysicalAddress::from(0x1000)).await;
        Ok(())
    }
    let config = Config {
        uinput: false,
        logical_address: LogicalAddressType::Playback,
        ..Config::default()
    };
    setup_dbus_test(cb, Some(config)).await
}

pub async fn wait_timeout<Fut, T>(method: Fut, timeout: Duration) -> anyhow::Result<T>
where
    Fut: Future<Output = T>,
{
    select! {
        _ = sleep(timeout) => bail!("Timeout reached"),
        o = method => Ok(o)
    }
}

#[tokio::test]
async fn test_no_caps() {
    async fn cb(_dev: ArcDevice) -> anyhow::Result<()> {
        Ok(())
    }
    assert!(setup_dbus_test(cb, None).await.is_err());
}

#[tokio::test]
async fn test_caps_transmit_only() {
    async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
        let mut dev = dev.lock().await;
        dev.set_caps(Capabilities::TRANSMIT);
        Ok(())
    }
    let test = setup_dbus_test(cb, None).await.unwrap();
    assert_eq!(test.proxy.physical_address().await.unwrap(), 0xFFFF);
    assert!(test.proxy.logical_addresses().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_caps_log_addr() {
    async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
        let mut dev = dev.lock().await;
        dev.set_caps(Capabilities::LOG_ADDRS | Capabilities::TRANSMIT);
        dev.set_phys_addr(PhysicalAddress::from(0x1000)).await;
        Ok(())
    }
    let test = setup_dbus_test(cb, None).await.unwrap();
    assert_eq!(test.proxy.physical_address().await.unwrap(), 0x1000);
    assert_eq!(
        test.proxy.logical_addresses().await.unwrap(),
        &[u8::from(LogicalAddress::PlaybackDevice1)]
    );
}

#[tokio::test]
async fn test_tx_no_log_addr() {
    async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
        let mut dev = dev.lock().await;
        dev.set_caps(Capabilities::TRANSMIT);
        Ok(())
    }
    let test = setup_dbus_test(cb, None).await.unwrap();
    assert!(test.proxy.logical_addresses().await.unwrap().is_empty());
    let err = test.proxy.standby(0).await.unwrap_err();
    let zbus::Error::MethodError(name, text, _) = err else {
        panic!();
    };
    assert_eq!(
        OwnedErrorName::try_from("com.steampowered.CecDaemon1.Error.NoLogicalAddress").unwrap(),
        name
    );
    assert_eq!(Some(Error::NoLogicalAddress.to_string()), text);
}

#[tokio::test]
async fn test_tx_invalid_log_addr() {
    async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
        let mut dev = dev.lock().await;
        dev.set_caps(Capabilities::LOG_ADDRS | Capabilities::TRANSMIT);
        dev.set_phys_addr(PhysicalAddress::from(0x1000)).await;
        dev.force_unregistered = true;
        Ok(())
    }
    let test = setup_dbus_test(cb, None).await.unwrap();
    assert_eq!(test.proxy.physical_address().await.unwrap(), 0x1000);
    assert_eq!(
        test.proxy.logical_addresses().await.unwrap(),
        &[u8::from(LogicalAddress::Unregistered)]
    );

    let err = test
        .proxy
        .standby(LogicalAddress::Tv.into())
        .await
        .unwrap_err();
    let zbus::Error::MethodError(name, text, _) = err else {
        panic!();
    };
    assert_eq!(
        OwnedErrorName::try_from("com.steampowered.CecDaemon1.Error.InvalidData").unwrap(),
        name
    );
    assert_eq!(Some(Error::InvalidData.to_string()), text);
}
