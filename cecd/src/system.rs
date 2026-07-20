/*
 * Copyright © 2024 Valve Software
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

use anyhow::{ensure, Result};
use input_linux::Key;
use linux_cec::device::{Capabilities, ConnectorInfo};
use linux_cec::operand::UiCommand;
use linux_cec::{self, FollowerMode, InitiatorMode, LogicalAddressType, PhysicalAddress, VendorId};
use nix::errno::Errno;
use nix::unistd::gethostname;
use std::collections::hash_map::{Entry, HashMap};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::read_dir;
use tokio::spawn;
use tokio::sync::broadcast::{channel, Receiver, Sender};
use tokio::sync::{Mutex, MutexGuard};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zbus::connection::Connection;
use zbus::fdo::ObjectManager;
use zbus::names::UniqueName;
use zbus::proxy;
use zbus::zvariant::ObjectPath;

use crate::config::{read_config_file, read_default_config, Config};
use crate::dbus::{CecConfig, CecDevice, Daemon, PATH};
use crate::device::DrmConnector;
use crate::message_handler::{MessageHandler, MessageHandlerTask};
use crate::ArcDevice;

const RESUME_WAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) struct System {
    osd_name: String,
    pub config: Config,
    config_path: Option<PathBuf>,

    pub connection: Connection,
    pub channel: Sender<SystemMessage>,
    system_bus: Connection,
    inhibit_fd: Option<zbus::zvariant::OwnedFd>,
    token: CancellationToken,
    devs: HashMap<PathBuf, CancellationToken>,
    standby: bool,
    resume_wake_deadline: Option<Instant>,

    message_handlers: HashMap<u8, MessageHandlerHandle>,
}

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LoginManager {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, sleep: bool) -> Result<()>;

    fn suspend(&self, interactive: bool) -> Result<()>;

    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> Result<zbus::zvariant::OwnedFd>;
}

#[derive(Debug, Clone)]
pub(crate) enum SystemMessage {
    Wake { wake_tv: bool, from_standby: bool },
    Standby { standby_tv: bool, force: bool },
    ReloadConfig,
    ReconfigureConnector(ConnectorInfo),
}

impl System {
    // Most of these mappings match Linux's rc mapping, but a few are intentionally
    // changed or removed in an opinionated way. These are just the defaults however,
    // so they are easily overridden or unmapped if desired.
    pub(crate) const DEFAULT_MAPPINGS: &[(UiCommand, Key)] = &[
        (UiCommand::Select, Key::Enter),
        (UiCommand::Up, Key::Up),
        (UiCommand::Down, Key::Down),
        (UiCommand::Left, Key::Left),
        (UiCommand::Right, Key::Right),
        (UiCommand::RightUp, Key::RightUp),
        (UiCommand::RightDown, Key::RightDown),
        (UiCommand::LeftUp, Key::LeftUp),
        (UiCommand::LeftDown, Key::LeftDown),
        (UiCommand::DeviceRootMenu, Key::RootMenu),
        (UiCommand::DeviceSetupMenu, Key::Setup),
        (UiCommand::ContentsMenu, Key::Menu),
        (UiCommand::FavoriteMenu, Key::Favorites),
        (UiCommand::Back, Key::Esc),
        (UiCommand::MediaTopMenu, Key::MediaTopMenu),
        (UiCommand::MediaContextSensitiveMenu, Key::ContextMenu),
        (UiCommand::NumberEntryMode, Key::Digits),
        (UiCommand::Number11, Key::Numeric11),
        (UiCommand::Number12, Key::Numeric12),
        (UiCommand::Number0OrNumber10, Key::Num0),
        (UiCommand::Number1, Key::Num1),
        (UiCommand::Number2, Key::Num2),
        (UiCommand::Number3, Key::Num3),
        (UiCommand::Number4, Key::Num4),
        (UiCommand::Number5, Key::Num5),
        (UiCommand::Number6, Key::Num6),
        (UiCommand::Number7, Key::Num7),
        (UiCommand::Number8, Key::Num8),
        (UiCommand::Number9, Key::Num9),
        (UiCommand::Dot, Key::Dot),
        (UiCommand::Enter, Key::Enter),
        (UiCommand::Clear, Key::Clear),
        (UiCommand::NextFavorite, Key::NextFavorite),
        (UiCommand::ChannelUp, Key::ChannelUp),
        (UiCommand::ChannelDown, Key::ChannelDown),
        (UiCommand::PreviousChannel, Key::Previous),
        (UiCommand::SoundSelect, Key::Sound),
        // UiCommand::InputSelect, no good mapping
        (UiCommand::DisplayInformation, Key::Info),
        (UiCommand::Help, Key::Help),
        (UiCommand::PageUp, Key::PageUp),
        (UiCommand::PageDown, Key::PageDown),
        (UiCommand::Power, Key::Power),
        (UiCommand::VolumeUp, Key::VolumeUp),
        (UiCommand::VolumeDown, Key::VolumeDown),
        (UiCommand::Mute, Key::Mute),
        (UiCommand::Play, Key::PlayCD),
        (UiCommand::Stop, Key::StopCD),
        (UiCommand::Pause, Key::PauseCD),
        (UiCommand::Record, Key::Record),
        (UiCommand::Rewind, Key::Rewind),
        (UiCommand::FastForward, Key::FastForward),
        (UiCommand::Eject, Key::EjectCD),
        (UiCommand::SkipForward, Key::NextSong),
        (UiCommand::SkipBackward, Key::PreviousSong),
        (UiCommand::StopRecord, Key::StopRecord),
        (UiCommand::PauseRecord, Key::PauseRecord),
        (UiCommand::Angle, Key::Angle),
        // UiCommand::SubPicture, no good mapping
        (UiCommand::VideoOnDemand, Key::Vod),
        (UiCommand::ElectronicProgramGuide, Key::EPG),
        (UiCommand::TimerProgramming, Key::Time),
        (UiCommand::InitialConfiguration, Key::Config),
        // UiCommand::SelectBroadcastType, no good mapping
        // UiCommand::SelectSoundPresentation, no good mapping
        (UiCommand::AudioDescription, Key::AudioDesc),
        (UiCommand::Internet, Key::WWW),
        (UiCommand::ThreeDMode, Key::Audio3dMode),
        // The "function" keys are intended for operations that have a well-
        // defined end state, e.g. "mute function" does not toggle mute, it
        // specifically enables mute. Since the majority aren't cleanly
        // mappable, these are left unmapped instead of having the possibility
        // of doing the wrong operation.
        // UiCommand::PlayFunction
        // UiCommand::PausePlayFunction
        // UiCommand::RecordFunction
        // UiCommand::PauseRecordFunction
        // UiCommand::StopFunction
        // UiCommand::MuteFunction
        (UiCommand::RestoreVolumeFunction, Key::Unmute),
        // UiCommand::TuneFunction
        // UiCommand::SelectMediaFunction
        // UiCommand::SelectAvInputFunction
        // UiCommand::SelectAudioInputFunction
        (UiCommand::PowerToggleFunction, Key::Power),
        (UiCommand::PowerOffFunction, Key::Sleep),
        (UiCommand::PowerOnFunction, Key::Wakeup),
        (UiCommand::F1Blue, Key::Blue),
        (UiCommand::F2Red, Key::Red),
        (UiCommand::F3Green, Key::Green),
        (UiCommand::F4Yellow, Key::Yellow),
        (UiCommand::F5, Key::F5),
        (UiCommand::Data, Key::Data),
    ];

    pub(crate) async fn new(
        token: CancellationToken,
        connection: Connection,
        system_bus: Connection,
        config_path: Option<PathBuf>,
    ) -> Result<System> {
        let (channel, _) = channel(10);

        let hostname = gethostname()
            .ok()
            .and_then(|hostname| hostname.into_string().ok());

        let osd_name = match hostname {
            Some(hostname) if !hostname.is_empty() => hostname,
            _ => String::from("CEC Device"),
        };

        let login_manager = LoginManagerProxy::new(&system_bus).await?;

        let inhibit_fd = login_manager
            .inhibit("sleep", "cecd", "Put TV to sleep", "delay")
            .await
            .inspect_err(|e| warn!("Could not register to delay suspend: {e}"))
            .ok();

        Ok(System {
            osd_name,
            config: Config::default(),
            connection,
            system_bus,
            inhibit_fd,
            token,
            devs: HashMap::new(),
            channel,
            config_path,
            standby: false,
            resume_wake_deadline: None,
            message_handlers: HashMap::new(),
        })
    }

    pub(crate) async fn find_devs(&mut self) -> Result<Vec<CecDevice>> {
        let mut devs = Vec::new();
        let mut add = HashMap::new();
        let mut dir = read_dir("/dev").await?;
        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("cec") {
                continue;
            }

            let path = entry.path();
            if self.devs.contains_key(&path) {
                continue;
            }

            let pathname = path.display();
            debug!("Scanning cec device {pathname}");

            let token = self.token.child_token();
            devs.push(CecDevice::open(&path, token.clone()).await?);
            info!("Found cec device at {pathname}");
            add.insert(path, token);
        }
        self.devs.extend(add);
        Ok(devs)
    }

    pub(crate) async fn find_dev(&mut self, path: impl AsRef<Path>) -> Result<CecDevice> {
        let pathname = path.as_ref().display();
        debug!("Scanning cec device {pathname}");
        ensure!(
            !self.devs.contains_key(path.as_ref()),
            "Device {pathname} already loaded"
        );
        let token = self.token.child_token();
        let dev = CecDevice::open(&path, token.clone()).await?;
        info!("Found cec device at {pathname}");
        self.devs.insert(path.as_ref().to_path_buf(), token);
        Ok(dev)
    }

    pub(crate) fn close_dev(&mut self, path: impl AsRef<Path>) {
        if let Some(token) = self.devs.remove(path.as_ref()) {
            token.cancel();
        }
    }

    pub(crate) async fn reconfig(&mut self) -> Result<()> {
        let config = if let Some(ref config_path) = self.config_path {
            read_config_file(config_path).await?
        } else {
            read_default_config().await?
        };
        self.set_config(config).await
    }

    pub(crate) async fn set_config(&mut self, config: Config) -> Result<()> {
        if let Some(ref osd_name) = config.osd_name {
            self.osd_name.clone_from(osd_name);
        }
        self.config = config;

        if self.config.logical_address == LogicalAddressType::Unregistered {
            self.config.logical_address = LogicalAddressType::Playback;
        }

        if self.config.osd_name.is_none() {
            let hostname = gethostname()
                .ok()
                .and_then(|hostname| hostname.into_string().ok());

            self.config.osd_name = Some(match hostname {
                Some(hostname) if !hostname.is_empty() => hostname,
                _ => String::from("CEC Device"),
            });
        }

        if self.config.mappings.is_empty() {
            self.config.mappings = System::DEFAULT_MAPPINGS.iter().copied().collect();
        }

        debug!("Configuration loaded: {:#?}", self.config);

        self.send_message(SystemMessage::ReloadConfig).await;
        Ok(())
    }

    async fn reconfig_connector(&mut self, connector: ConnectorInfo) {
        self.send_message(SystemMessage::ReconfigureConnector(connector))
            .await;
    }

    pub(crate) fn subscribe(&self) -> Receiver<SystemMessage> {
        self.channel.subscribe()
    }

    fn trimmed_osd_name(&self) -> &str {
        if self.osd_name.len() <= 14 {
            return self.osd_name.as_str();
        }
        // TODO: Simplify using floor_char_boundary when we can bump the minimum rust ver to 1.91
        for i in (10..=14).rev() {
            if self.osd_name.is_char_boundary(i) {
                return &self.osd_name.as_str()[..i];
            }
        }
        unreachable!();
    }

    pub(crate) async fn configure_dev(
        &mut self,
        device: ArcDevice,
        connector: Option<&DrmConnector>,
    ) -> Result<Option<DrmConnector>> {
        let device = device.lock().await;
        let driver_name = device.get_driver_name().await?;
        let driver_name = driver_name.to_string_lossy();
        let adapter_name = device.get_adapter_name().await?;
        let adapter_name = adapter_name.to_string_lossy();
        let caps = device.get_capabilities().await?;
        let conn_info = if let Some(connector) = connector {
            connector.to_connector_info().await?
        } else if caps.contains(Capabilities::CONNECTOR_INFO) {
            device.get_connector_info().await?
        } else {
            ConnectorInfo::None
        };
        let mut new_connector = None;
        device.set_initiator_mode(InitiatorMode::Enabled).await?;
        debug!("Device driver: {driver_name}");
        debug!("Device adapter: {adapter_name}");
        debug!("Device has caps: {caps:?}");
        if caps.contains(Capabilities::PHYS_ADDR) {
            if let Some(mut connector) = connector.cloned() {
                connector.reload_physical_address().await?;
                debug!(
                    "Found physical address {} in EDID for card {} connector {}",
                    connector.physical_address, connector.card, connector.connector
                );
                device
                    .set_physical_address(connector.physical_address)
                    .await?;
                new_connector = Some(connector);
            } else if let Ok(Some(connector)) = System::find_physical_address().await {
                debug!(
                    "Found physical address {} in EDID for card {} connector {}",
                    connector.physical_address, connector.card, connector.connector
                );
                device
                    .set_physical_address(connector.physical_address)
                    .await?;
                new_connector = Some(connector);
            } else if let Some(physical_address) = self.config.physical_address {
                debug!(
                    "Physical address required but not found, using fallback {}",
                    physical_address
                );
                device.set_physical_address(physical_address).await?;
            } else {
                warn!("Couldn't determine physical address");
                device
                    .set_physical_address(PhysicalAddress::default())
                    .await?;
            }
        } else if let ConnectorInfo::DrmConnector {
            card_no,
            connector_id,
        } = conn_info
        {
            let physical_address = device.get_physical_address().await?;
            new_connector = Some(
                DrmConnector::from_connector_info(
                    ConnectorInfo::DrmConnector {
                        card_no,
                        connector_id,
                    },
                    physical_address,
                )
                .await?,
            );
        }
        if caps.contains(Capabilities::LOG_ADDRS) {
            for i in 0..5 {
                match device.clear_logical_addresses().await {
                    Err(linux_cec::Error::SystemError(e)) if e == Errno::EBUSY => {
                        if i == 4 {
                            return Err(e.into());
                        }
                        sleep(Duration::from_millis(200)).await;
                    }
                    Err(e) => return Err(e.into()),
                    Ok(()) => break,
                }
            }
            device.set_osd_name(self.trimmed_osd_name()).await?;
            device.set_vendor_id(self.config.vendor_id).await?;
            device
                .set_logical_address(self.config.logical_address)
                .await?;
        }
        device.set_follower_mode(FollowerMode::Enabled).await?;
        Ok(new_connector)
    }

    async fn send_message(&mut self, message: SystemMessage) {
        // This is allowed to fail silently
        let _ = self.channel.send(message);
    }

    fn take_resume_wake(&mut self) -> bool {
        let Some(deadline) = self.resume_wake_deadline.take() else {
            return false;
        };
        Instant::now() < deadline
    }

    async fn find_physical_address() -> Result<Option<DrmConnector>> {
        let mut adapter = None;
        let mut dir = read_dir("/sys/class/drm").await?;
        while let Some(entry) = dir.next_entry().await? {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !file_name.starts_with("card") || !file_name.contains('-') {
                continue;
            }
            let path = entry.path();
            let Some(connector) = DrmConnector::from_drm_path(path).await? else {
                continue;
            };
            if adapter.is_some() {
                debug!("Found multiple connected monitors with physical addresses");
                return Ok(None);
            }
            adapter = Some(connector);
        }
        Ok(adapter)
    }
}

#[derive(Clone, Debug)]
#[repr(transparent)]
pub(crate) struct SystemHandle(pub Arc<Mutex<System>>);

impl SystemHandle {
    pub(crate) async fn lock(&self) -> MutexGuard<'_, System> {
        self.0.lock().await
    }

    pub(crate) async fn osd_name(&self) -> String {
        self.lock().await.osd_name.clone()
    }

    pub(crate) async fn vendor_id(&self) -> Option<VendorId> {
        self.lock().await.config.vendor_id
    }

    pub(crate) async fn find_devs(&self) -> Result<Vec<CancellationToken>> {
        let mut tokens = Vec::new();
        let devs;
        let connection;
        {
            let mut system = self.lock().await;
            devs = system.find_devs().await?;
            connection = system.connection.clone();
        }
        for dev in devs {
            tokens.push(dev.token.clone());
            dev.register(connection.clone(), self.clone()).await?;
        }
        Ok(tokens)
    }

    pub(crate) async fn find_dev(&self, path: impl AsRef<Path>) -> Result<CancellationToken> {
        let dev;
        let connection;
        {
            let mut system = self.lock().await;
            dev = system.find_dev(path).await?;
            connection = system.connection.clone();
        }
        let token = dev.token.clone();
        let dbus_path = dev.dbus_path().to_owned();
        dev.register(connection.clone(), self.clone()).await?;
        let wake_tv = self.lock().await.take_resume_wake();
        if wake_tv {
            debug!("Waking TV after CEC device registered following resume");
            let interface = connection
                .object_server()
                .interface::<_, CecDevice>(dbus_path.as_ref())
                .await?;
            interface
                .get()
                .await
                .send_system_message(SystemMessage::Wake {
                    wake_tv: true,
                    from_standby: true,
                })
                .await?;
        }
        Ok(token)
    }

    pub(crate) async fn close_dev(&self, path: impl AsRef<Path>) {
        let mut system = self.lock().await;
        system.close_dev(path);
    }

    pub(crate) async fn reconfig(&self) -> Result<()> {
        let mut system = self.lock().await;
        system.reconfig().await
    }

    pub(crate) async fn reconfig_connector(&self, connector: ConnectorInfo) {
        let mut system = self.lock().await;
        system.reconfig_connector(connector).await;
    }

    pub(crate) async fn run(&mut self) -> Result<()> {
        let login_manager = LoginManagerProxy::new(&self.lock().await.system_bus).await?;
        let mut sleep_stream = login_manager.receive_prepare_for_sleep().await?;
        loop {
            let Some(sleep_message) = sleep_stream.next().await else {
                continue;
            };
            let sleep = match sleep_message.args() {
                Ok(args) => args.sleep,
                Err(e) => {
                    warn!("Failed to receive PrepareForSleep message from logind: {e}");
                    continue;
                }
            };
            let mut system = self.lock().await;
            if !sleep {
                system.standby = false;
                let wake_tv = system.config.wake_tv;
                system.resume_wake_deadline = wake_tv.then(|| Instant::now() + RESUME_WAKE_TIMEOUT);
                debug!(
                    "Woke from standby. {} TV.",
                    if wake_tv { "Waking" } else { "Not waking" }
                );
                system
                    .send_message(SystemMessage::Wake {
                        wake_tv,
                        from_standby: true,
                    })
                    .await;

                // Reacquire sleep delay inhbitor lock
                system.inhibit_fd = login_manager
                    .inhibit("sleep", "cecd", "Put TV to sleep", "delay")
                    .await
                    .inspect_err(|e| warn!("Could not register to delay suspend: {e}"))
                    .ok();
            } else {
                // Don't attempt to put the TV to sleep if it's already putting us to sleep
                let standby_tv = system.config.suspend_tv && !system.standby;
                debug!(
                    "Entering standby. {} TV in standby.",
                    if standby_tv { "Putting" } else { "Not putting" }
                );
                system
                    .send_message(SystemMessage::Standby {
                        standby_tv,
                        force: false,
                    })
                    .await;

                system.standby = true;

                // Close the fd, release the lock
                system.inhibit_fd.take();
            }
        }
    }

    pub(crate) async fn suspend(&self) -> Result<()> {
        {
            let mut system = self.lock().await;
            if system.standby {
                return Ok(());
            }
            system.standby = true;
        }

        let login_manager = LoginManagerProxy::new(&self.lock().await.system_bus).await?;
        login_manager.suspend(false).await
    }

    pub(crate) async fn setup_dbus(&self) -> Result<()> {
        // We can't have the lock held while we install
        // the ObjectManager to avoid a deadlock
        let connection = self.lock().await.connection.clone();
        let object_server = connection.object_server();

        let daemon_obj = Daemon::new(self);
        object_server
            .at(format!("{PATH}/Daemon"), daemon_obj)
            .await?;

        object_server.at(PATH, ObjectManager {}).await?;
        Ok(())
    }

    pub(crate) async fn get_message_handler(&self, opcode: u8) -> Option<MessageHandler<'_>> {
        let system = self.lock().await;
        system
            .message_handlers
            .get(&opcode)
            .map(|handle| handle.handler.clone())
    }

    pub(crate) async fn register_message_handler(
        &self,
        opcode: impl Into<u8>,
        object: ObjectPath<'_>,
        bus_name: &UniqueName<'_>,
    ) -> Result<bool> {
        let mut system = self.lock().await;
        let opcode = opcode.into();
        if system.message_handlers.contains_key(&opcode) {
            return Ok(false);
        }
        let task =
            MessageHandlerTask::start(&system.connection, self.clone(), opcode, bus_name).await?;
        let handler = MessageHandler::new(&system.connection, &object, bus_name).await?;
        system
            .message_handlers
            .insert(opcode, MessageHandlerHandle { handler, task });
        Ok(true)
    }

    pub(crate) async fn unregister_message_handler(
        &self,
        opcode: impl Into<u8>,
        bus_name: &UniqueName<'_>,
    ) -> Result<bool> {
        let mut system = self.lock().await;
        let opcode = opcode.into();
        let entry = system.message_handlers.entry(opcode);
        let handle = match entry {
            Entry::Occupied(entry) if entry.get().handler.is_name(bus_name) => entry.remove(),
            _ => return Ok(false),
        };
        handle.task.abort();
        Ok(true)
    }

    pub(crate) async fn list_handled_messages(&self) -> HashSet<u8> {
        self.lock().await.message_handlers.keys().copied().collect()
    }

    pub(crate) async fn wake_all(&self) {
        let mut system = self.lock().await;
        system
            .send_message(SystemMessage::Wake {
                wake_tv: true,
                from_standby: false,
            })
            .await;
    }

    pub(crate) async fn standby_all(&self, force: bool) {
        let mut system = self.lock().await;
        system
            .send_message(SystemMessage::Standby {
                standby_tv: true,
                force,
            })
            .await;
    }
}

#[derive(Debug)]
pub struct ConfigTask {
    channel: Receiver<SystemMessage>,
    connection: Connection,
}

impl ConfigTask {
    pub async fn start(system: SystemHandle) -> Result<JoinHandle<Result<()>>> {
        let channel;
        let connection;
        {
            let system = system.lock().await;
            channel = system.subscribe();
            connection = system.connection.clone();
        }
        let config_obj = CecConfig::new(system).await;
        connection
            .object_server()
            .at(format!("{PATH}/Daemon"), config_obj)
            .await?;
        let task = ConfigTask {
            channel,
            connection,
        };
        Ok(spawn(task.run()))
    }

    async fn run(mut self) -> Result<()> {
        let config_obj = self
            .connection
            .object_server()
            .interface::<_, CecConfig>(format!("{PATH}/Daemon"))
            .await?;
        loop {
            let message = match self.channel.recv().await {
                Ok(message) => message,
                Err(e) => {
                    warn!("Error receiving message: {e}");
                    return Err(e.into());
                }
            };
            if !matches!(message, SystemMessage::ReloadConfig) {
                continue;
            }
            let emitter = config_obj.signal_emitter();
            config_obj.get_mut().await.reconfigure(emitter).await;
        }
    }
}

#[derive(Debug)]
pub struct MessageHandlerHandle {
    handler: MessageHandler<'static>,
    task: JoinHandle<Result<()>>,
}

#[cfg(test)]
mod test {
    use super::*;

    use linux_cec::message::{Message, Opcode};
    use linux_cec::operand::AbortReason;
    use linux_cec::{LogicalAddress, PhysicalAddress};
    use std::time::Duration;
    use tokio::time::sleep;
    use zbus::zvariant::OwnedObjectPath;
    use zbus::{fdo, interface};

    use crate::testing::{setup_dbus_test, wait_timeout};

    #[derive(Debug, PartialEq)]
    struct RemoteMessage {
        device: OwnedObjectPath,
        initiator: u8,
        destination: u8,
        timestamp: u64,
        message: Vec<u8>,
    }

    #[derive(Debug)]
    struct MockMessageHandler {
        last_message: Option<RemoteMessage>,
        abort: Option<AbortReason>,
        channel: Sender<()>,
        timeout: bool,
    }

    impl MockMessageHandler {
        fn new() -> MockMessageHandler {
            MockMessageHandler {
                last_message: None,
                abort: None,
                channel: Sender::new(2),
                timeout: false,
            }
        }
    }

    #[interface(name = "com.steampowered.CecDaemon1.MessageHandler1")]
    impl MockMessageHandler {
        async fn handle_message(
            &mut self,
            device: OwnedObjectPath,
            initiator: u8,
            destination: u8,
            timestamp: u64,
            message: &[u8],
        ) -> fdo::Result<(bool, u8)> {
            self.last_message = Some(RemoteMessage {
                device,
                initiator,
                destination,
                timestamp,
                message: message.to_vec(),
            });
            let _ = self.channel.send(());
            if self.timeout {
                sleep(Duration::from_millis(100)).await;
                let _ = self.channel.send(());
                Ok((true, 0))
            } else if let Some(abort) = self.abort.as_ref() {
                Ok((false, *abort as u8))
            } else {
                Ok((true, 0))
            }
        }
    }

    async fn cb(dev: ArcDevice) -> anyhow::Result<()> {
        let mut dev = dev.lock().await;
        dev.set_caps(Capabilities::LOG_ADDRS | Capabilities::TRANSMIT);
        dev.set_phys_addr(PhysicalAddress::from(0x1000)).await;
        Ok(())
    }

    #[tokio::test]
    async fn test_no_handlers() {
        let test = setup_dbus_test(cb, None).await.unwrap();

        assert!(test.system.list_handled_messages().await.is_empty());
    }

    #[tokio::test]
    async fn test_resume_wake_is_one_time() {
        let test = setup_dbus_test(cb, None).await.unwrap();
        let mut system = test.system.lock().await;

        system.resume_wake_deadline = Some(Instant::now() + RESUME_WAKE_TIMEOUT);
        assert!(system.take_resume_wake());
        assert!(!system.take_resume_wake());

        system.resume_wake_deadline = Some(Instant::now());
        assert!(!system.take_resume_wake());
    }

    #[tokio::test]
    async fn test_register_handler_drop() {
        let test = setup_dbus_test(cb, None).await.unwrap();
        let new_conn = test.dbus.new_connection().await.unwrap();

        let object_server = new_conn.object_server();
        let path = ObjectPath::try_from("/TestHandler").unwrap();
        assert!(object_server
            .at(&path, MockMessageHandler::new())
            .await
            .unwrap());
        let unique_name = new_conn.unique_name().unwrap();

        assert!(test
            .system
            .register_message_handler(Opcode::GetCecVersion, path.as_ref(), unique_name)
            .await
            .unwrap());
        assert_eq!(
            test.system
                .list_handled_messages()
                .await
                .into_iter()
                .collect::<Vec<_>>(),
            &[Opcode::GetCecVersion as u8]
        );

        new_conn.graceful_shutdown().await;
        sleep(Duration::from_millis(5)).await; // Wait for the drop to propagate
        assert!(test.system.list_handled_messages().await.is_empty());
    }

    #[tokio::test]
    async fn test_register_handler_relay() {
        let test = setup_dbus_test(cb, None).await.unwrap();
        let new_conn = test.dbus.new_connection().await.unwrap();

        let object_server = new_conn.object_server();
        let path = ObjectPath::try_from("/TestHandler").unwrap();
        let handler = MockMessageHandler::new();
        let mut rx = handler.channel.subscribe();
        assert!(object_server.at(&path, handler).await.unwrap());
        let handler = object_server
            .interface::<_, MockMessageHandler>(&path)
            .await
            .unwrap();
        let unique_name = new_conn.unique_name().unwrap();

        assert!(test
            .system
            .register_message_handler(Opcode::GetCecVersion, path.as_ref(), unique_name)
            .await
            .unwrap());

        let notify = test
            .dev
            .lock()
            .await
            .send_rx_message(Message::GetCecVersion {}, LogicalAddress::Tv)
            .await;
        notify.notified().await;
        wait_timeout(rx.recv(), Duration::from_millis(50))
            .await
            .unwrap()
            .unwrap();
        let message = handler.get_mut().await.last_message.take().unwrap();
        assert_eq!(&message.device.as_ref(), test.proxy.inner().path());
        assert_eq!(message.initiator, LogicalAddress::Tv as u8);
        assert_eq!(message.destination, LogicalAddress::PlaybackDevice1 as u8);
        assert_eq!(message.message, &[Opcode::GetCecVersion as u8]);
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
    }

    #[tokio::test]
    async fn test_register_handler_timeout() {
        let test = setup_dbus_test(cb, None).await.unwrap();
        let new_conn = test.dbus.new_connection().await.unwrap();

        let object_server = new_conn.object_server();
        let path = ObjectPath::try_from("/TestHandler").unwrap();
        let mut handler = MockMessageHandler::new();
        let mut rx = handler.channel.subscribe();
        handler.timeout = true;
        assert!(object_server.at(&path, handler).await.unwrap());
        let unique_name = new_conn.unique_name().unwrap();

        assert!(test
            .system
            .register_message_handler(Opcode::GetCecVersion, path.as_ref(), unique_name)
            .await
            .unwrap());

        let notify = test
            .dev
            .lock()
            .await
            .send_rx_message(Message::GetCecVersion {}, LogicalAddress::Tv)
            .await;
        notify.notified().await;
        rx.recv().await.unwrap();
        assert!(test.dev.lock().await.dequeue_tx_message().await.is_none());
        rx.recv().await.unwrap();
        let (message, address) = test.dev.lock().await.dequeue_tx_message().await.unwrap();
        assert_eq!(address, LogicalAddress::Tv);
        assert_eq!(
            message,
            Message::FeatureAbort {
                opcode: Opcode::GetCecVersion as u8,
                abort_reason: AbortReason::Undetermined,
            }
        );
    }

    #[tokio::test]
    async fn test_register_handler_abort() {
        let test = setup_dbus_test(cb, None).await.unwrap();
        let new_conn = test.dbus.new_connection().await.unwrap();

        let object_server = new_conn.object_server();
        let path = ObjectPath::try_from("/TestHandler").unwrap();
        let mut handler = MockMessageHandler::new();
        let mut rx = handler.channel.subscribe();
        handler.abort = Some(AbortReason::Refused);
        assert!(object_server.at(&path, handler).await.unwrap());
        let unique_name = new_conn.unique_name().unwrap();

        assert!(test
            .system
            .register_message_handler(Opcode::GetCecVersion, path.as_ref(), unique_name)
            .await
            .unwrap());

        let notify = test
            .dev
            .lock()
            .await
            .send_rx_message(Message::GetCecVersion {}, LogicalAddress::Tv)
            .await;
        notify.notified().await;
        rx.recv().await.unwrap();
        sleep(Duration::from_millis(1)).await;
        let (message, address) = test.dev.lock().await.dequeue_tx_message().await.unwrap();
        assert_eq!(address, LogicalAddress::Tv);
        assert_eq!(
            message,
            Message::FeatureAbort {
                opcode: Opcode::GetCecVersion as u8,
                abort_reason: AbortReason::Refused,
            }
        );
    }

    #[tokio::test]
    async fn test_double_register_handler() {
        let test = setup_dbus_test(cb, None).await.unwrap();
        let new_conn = test.dbus.new_connection().await.unwrap();

        let object_server = new_conn.object_server();
        let unique_name = new_conn.unique_name().unwrap();

        let path = ObjectPath::try_from("/TestHandler").unwrap();
        assert!(object_server
            .at(&path, MockMessageHandler::new())
            .await
            .unwrap());
        assert!(test
            .system
            .register_message_handler(Opcode::GetCecVersion, path.as_ref(), unique_name)
            .await
            .unwrap());

        let path = ObjectPath::try_from("/TestHandler2").unwrap();
        assert!(object_server
            .at(&path, MockMessageHandler::new())
            .await
            .unwrap());
        assert!(!test
            .system
            .register_message_handler(Opcode::GetCecVersion, path.as_ref(), unique_name)
            .await
            .unwrap());

        assert_eq!(
            test.system
                .list_handled_messages()
                .await
                .into_iter()
                .collect::<Vec<_>>(),
            &[Opcode::GetCecVersion as u8]
        );

        assert!(test
            .system
            .register_message_handler(Opcode::CecVersion, path.as_ref(), unique_name)
            .await
            .unwrap());

        assert_eq!(
            test.system.list_handled_messages().await,
            HashSet::from([Opcode::CecVersion as u8, Opcode::GetCecVersion as u8])
        );
    }
}
