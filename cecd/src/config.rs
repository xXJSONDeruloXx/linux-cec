/*
 * Copyright © 2024 Valve Software
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

use anyhow::Result;
use async_trait::async_trait;
use config::builder::AsyncState;
use config::{
    AsyncSource, ConfigBuilder, ConfigError, FileFormat, FileStoredFormat, Format, Map, Value,
};
use input_linux::Key;
use linux_cec::operand::UiCommand;
use linux_cec::{LogicalAddressType, PhysicalAddress, VendorId};
use serde::de::{self, Unexpected};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tokio::fs::{read_dir, read_to_string};
use tracing::{debug, error};
use xdg::BaseDirectories;

fn de_mappings<'de, D>(deserializer: D) -> Result<HashMap<UiCommand, Key>, D::Error>
where
    D: Deserializer<'de>,
{
    let string_map = HashMap::<String, u16>::deserialize(deserializer)?;
    let mut mappings = HashMap::new();
    for (k, v) in string_map {
        let Ok(key) = UiCommand::from_str(&k) else {
            return Err(de::Error::invalid_value(
                Unexpected::Str(&k),
                &"an HDMI CEC UI command name",
            ));
        };
        let Ok(value) = Key::try_from(v) else {
            return Err(de::Error::invalid_value(
                Unexpected::Unsigned(v.into()),
                &"a Linux input event key value",
            ));
        };
        mappings.insert(key, value);
    }

    Ok(mappings)
}

fn de_physical_address_default() -> Option<PhysicalAddress> {
    None
}

fn de_true() -> bool {
    true
}

#[derive(Deserialize, Clone, Debug, Default)]
pub(crate) struct Config {
    /// The default advertised OSD name for this device, max 14 bytes. If not
    /// set, it uses the hostname, or "CEC Device" if it can't get the hostname.
    #[serde(default)]
    pub osd_name: Option<String>,
    /// The vendor OUI for this device. Defaults to `None`.
    #[serde(default)]
    pub vendor_id: Option<VendorId>,
    /// The type of logical address this device should request. Defaults to `playback`.
    #[serde(default)]
    pub logical_address: LogicalAddressType,
    /// The requested physical address. If the device offloads this to the OS and
    /// we're unable to determine the correct one, use this.
    #[serde(default = "de_physical_address_default")]
    pub physical_address: Option<PhysicalAddress>,
    /// Desired key mappings for uinput. Defaults are found in `system.rs`.
    #[serde(deserialize_with = "de_mappings", default)]
    pub mappings: HashMap<UiCommand, Key>,
    /// Should cecd attempt to wake the TV when the device is woken? Defaults to false.
    #[serde(default)]
    pub wake_tv: bool,
    /// Should cecd attempt to suspend the TV when the device is suspended? Defaults to false.
    #[serde(default)]
    pub suspend_tv: bool,
    /// Should cecd announce that it is no longer the active source when the device is suspended?
    /// Defaults to true.
    #[serde(default = "de_true")]
    pub inactive_source_on_suspend: bool,
    /// Should cecd attempt to suspend when receiving a Standby command? Defaults to false.
    #[serde(default)]
    pub allow_standby: bool,
    /// Should uinput mappings be enabled. Defaults to true.
    #[serde(default = "de_true")]
    pub uinput: bool,
    /// Should cecd attempt to determine if it's the active source when it wakes up or starts.
    /// In theory we should always have this enabled, but due to shortcomings of the HDMI-CEC
    /// protocol, it is unreliable and can erroneously determine we're active when we're not.
    /// Defaults to true.
    #[serde(default = "de_true")]
    pub request_active_source: bool,
}

#[derive(Debug)]
struct AsyncFileSource<F: Format, P: AsRef<Path> + Sized + Send + Sync> {
    path: P,
    format: F,
}

impl<F: Format, P: AsRef<Path> + Sized + Send + Sync + Debug> AsyncFileSource<F, P> {
    fn from(path: P, format: F) -> AsyncFileSource<F, P> {
        AsyncFileSource { path, format }
    }
}

#[async_trait]
impl<F: Format + Send + Sync + Debug, P: AsRef<Path> + Sized + Send + Sync + Debug> AsyncSource
    for AsyncFileSource<F, P>
{
    async fn collect(&self) -> Result<Map<String, Value>, ConfigError> {
        let path = self.path.as_ref();
        let text = match read_to_string(&path).await {
            Ok(text) => text,
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    debug!("No config file {} found", path.to_string_lossy());
                    return Ok(Map::new());
                }
                return Err(ConfigError::Foreign(Box::new(e)));
            }
        };
        let path = path.to_string_lossy().to_string();
        debug!("Config file {} read", path);
        self.format
            .parse(Some(&path), &text)
            .map_err(ConfigError::Foreign)
    }
}

async fn read_config_directory<P: AsRef<Path> + Sync + Send>(
    builder: ConfigBuilder<AsyncState>,
    path: P,
    extensions: &[&str],
    format: FileFormat,
) -> Result<ConfigBuilder<AsyncState>> {
    let mut dir = match read_dir(&path).await {
        Ok(dir) => dir,
        Err(e) => {
            if e.kind() == ErrorKind::NotFound {
                debug!(
                    "No config fragment directory {} found",
                    path.as_ref().to_string_lossy()
                );
                return Ok(builder);
            }
            error!(
                "Error reading config fragment directory {}: {e}",
                path.as_ref().to_string_lossy()
            );
            return Err(e.into());
        }
    };
    let mut entries = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
            if extensions.contains(&ext) {
                entries.push(path);
            }
        }
    }
    entries.sort();
    Ok(entries.into_iter().fold(builder, |builder, path| {
        builder.add_async_source(AsyncFileSource::from(path, format))
    }))
}

pub(crate) async fn read_default_config() -> Result<Config> {
    let mut builder = ConfigBuilder::<AsyncState>::default();
    let system_config_path = PathBuf::from("/usr/share/cecd");
    let etc_config_path = PathBuf::from("/etc/cecd");
    let mut config_paths = vec![system_config_path, etc_config_path];
    if let Some(home) = BaseDirectories::new().get_config_home() {
        config_paths.push(home.join("cecd"));
    }

    for config_path in config_paths {
        builder = builder.add_async_source(AsyncFileSource::from(
            config_path.join("config.toml"),
            FileFormat::Toml,
        ));
        builder = read_config_directory(
            builder,
            config_path.join("config.d"),
            FileFormat::Toml.file_extensions(),
            FileFormat::Toml,
        )
        .await?;
    }

    let config = builder.build().await?;
    Ok(config.try_deserialize()?)
}

pub(crate) async fn read_config_file(path: impl AsRef<Path>) -> Result<Config> {
    let builder = ConfigBuilder::<AsyncState>::default();
    let builder = builder.add_async_source(AsyncFileSource::from(
        path.as_ref().to_path_buf(),
        FileFormat::Toml,
    ));
    let config = builder.build().await?;
    Ok(config.try_deserialize()?)
}
