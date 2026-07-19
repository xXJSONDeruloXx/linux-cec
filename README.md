linux-cec
=========

linux-cec is a collection of Rust crates intended for interfacing with Linux's
[Consumer Electronics Control](https://en.wikipedia.org/Consumer_Electronics_Control) userspace subsystem:

- linux-cec: A library for interacting with CEC devices via adapters connected to the host.
- linux-cec-sys: Interfaces relating to the underlying ioctl API for /dev/cec* nodes, translated from
  `include/linux/cec.h` into Rust. You will probably not need to use these, but they are separated out for developers
  who don't wish to use linux-cec directly.
- linux-cec-macros: A private library used for implementing various aspects of the linux-cec crate.
  You should not use this directly, and no stability is guaranteed between versions.
- cecd: A daemon that runs on the host and exposes high level control for CEC over DBus, optionally exposing an input
  device for the remote control via [uinput](https://www.kernel.org/doc/html/latest/input/uinput.html).
- cecd-proxy: A set of zbus proxies for third-party crates to interface with a running instance of cecd over DBus.

Note that most GPUs do not expose CEC to the OS, and as such you may need an adapter supported by the kernel to use
CEC.  Please see the Linux admin guide [page on CEC](https://docs.kernel.org/admin-guide/media/cec.html) for more
details on supported devices. Modified versions of these files can be found in the accompanying [inputattach-cec-units
repo](https://gitlab.steamos.cloud/holo/inputattach-cec-units). In theory any device supported by Linux's CEC subsystem
should be supported by linux-cec, though only a small handful of devices have been tested so far.

## Why not libCEC FFI?

[LibCEC](http://libcec.pulse-eight.com/) has been the de facto library for interacting with CEC devices across multiple
operating systems for years. However, there are some significant shortcomings with the library. The most important one
is that the library has been unmaintained for years, with minimal support since 2020, and is rife with issues that
could use tackling. However, forking the library is hampered by the fact that libCEC is itself claimed to be a
registered trademark of Pulse-Eight per the source. The library itself is also GPL2+, which restricts the range of
software that can directly use it.

Some of these issues apply specifically to the Linux CEC API. For example, it's hardcoded to only support /dev/cec0,
and does not support hotplugging. If the device is disconnected while in use by an application, a thread will just
continuously poll the device, get an error, and then try again. This cannot be programmatically detected except for
scanning log messages. This is anything but robust, and there wasn't really a good way to fix this without some massive
overhauls.

Given all of these factors, it seemed like a fresh start specifically targeting the Linux subsystem instead of a more
pluggable system. Many of the devices libCEC supports have since gotten kernel drivers as well, so the need for a
userspace library handling it all directly has diminished since libCEC was started.

## cecd

Many features of CEC make sense at a system-wide level instead of an application level. For example, handling
wake/suspend synchronization of the device and the TV. Furthermore, some application-level features can make sense to
expose without directly using the library, such as mapping remote control button presses to key inputs. Cecd aims to
support all of these features:

- Optional support for waking the TV when coming out of suspend and putting the TV to sleep when entering suspend.
- Translating a configurable set of UI buttons (usually remote controller buttons) to an evdev input device.
- A DBus service for sending and receiving messages, including a handful of convenience methods.

### Configuration

cecd has configuration files located at various paths. They are loaded in the following order, with ones loaded later
overriding ones loaded earlier:

- `/usr/share/cecd/config.toml`: The system default configuration file.
- `/usr/share/cecd/config.d/*.toml`: Configuration fragments, for system packages that may want to tweak only portions
  of the configuration.
- `/etc/cecd/config.toml`: Editable system-wide configuration file.
- `/etc/cecd/config.d/*.toml`: Editable system-wide configuration fragments.
- `$XDG_CONFIG_HOME/cecd/config.toml`: User-specific configuration file.
- `$XDG_CONFIG_HOME/cecd/config.d/*.toml`: User-specific configuration fragments.

The configuration files are stored in [TOML format](https://toml.io/en/), with the following keys valid:

- `osd_name`: The default advertised OSD name for this device, max 14 bytes. Note that this is not guaranteed to work
  with non-ASCII characters with all TVs and may have unexpected results if attempted. Defaults to "CEC Device".
- `vendor_id`: The vendor [OUI](https://en.wikipedia.org/wiki/Organizationally_unique_identifier) for this device.
  Defaults to `None`.
- `logical_address`: The type of logical address this device should request. Valid types are `record`, `tuner`,
  `playback`, `audiosystem`, and `specific`. Defaults to `playback`.
- `physical_address`: The requested physical address. In theory you shouldn't need this, but some adapters offload this
  to the OS and we're not necessarily able to properly correlate adapter with the display and determine the correct
  device. In this case, we still need a physical address for the device to work. This is extremely error-prone so you
  shouldn't mess with this unless you need to, for example if this device is in an environment where the connections to
  the TV and any HDMI switches isn't changing. By default, no physical address is provided, so cecd won't try to
  configure one.
- `mappings`: Desired key mappings for uinput. This is a table of the CEC-specified UI Command names (listed below) to
  [Linux key code values](https://web.git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/include/uapi/linux/input-event-codes.h).
  Note that the key code value must be the numeric value, not the name. Defaults are listed below, and overriding the
  table at all will unset all of the default mappings.
- `wake_tv`: Should cecd attempt to wake the TV when the device is woken? Defaults to false.
- `suspend_tv`: Should cecd attempt to suspend the TV when the device is suspended? Defaults to false.
- `inactive_source_on_suspend`: Should cecd announce that it is no longer active when suspended? Defaults to true.
- `allow_standby`: Should cecd attempt to suspend when receiving a Standby command? Defaults to false.
- `uinput`: Should uinput mappings be enabled. Defaults to true.
- `request_active_source`: Should cecd attempt to determine if it's the active source when it wakes up or starts. In
  theory we should always have this enabled, but due to shortcomings of the HDMI-CEC protocol, it is unreliable and
  can erroneously determine we're active when we're not. Defaults to true.

#### UI Commands

CEC specifies a large list of "UI Commands" that can be sent to devices to tell them to do various things. What the
devices do is not well-specified for most commands, excluding the "function" commands, and various devices have been
known to behave very differently. As the "function" commands have specific definitions of their behaviors it is
recommended to use them carefully, if at all, and most are left unmapped by default. Some commands are specified to be
relayed from remote contols, which are marked in **bold**, though other commands may be relayed too. The full list, as
usable in cectool and cecd mapping names, is as follows, with the default mapping in cecd listed. Some commands have
multiple valid names, which are separated by commas. A few commands also optionally take additional parameters. The
parameters are marked in brackets.

- **select**, **ok** (This is generally marked as "OK", "Enter", or "Select" on remote controls.): `KEY_ENTER`
- **up**: `KEY_UP`
- **down**: `KEY_DOWN`
- **left**: `KEY_LEFT`
- **right**: `KEY_RIGHT`
- **right-up**: `KEY_RIGHT_UP`
- **right-down**: `KEY_RIGHT_DOWN`
- **left-up**: `KEY_LEFT_UP`
- **left-down**: `KEY_LEFT_DOWN`
- device-root-menu: `KEY_ROOT_MENU`
- device-setup-menu: `KEY_SETUP`
- contents-menu: `KEY_MENU`
- favorite-menu: `KEY_FAVORITES`
- **back**, **exit**: `KEY_ESC`
- media-top-menu: `KEY_MEDIA_TOP_MENU`
- media-context-sensitive-menu: `KEY_CONTEXT_MENU`,
- **number-entry-mode**: `KEY_DIGITS`
- **11**: `KEY_NUMERIC_11`
- **12**: `KEY_NUMERIC_12`
- **0**, **10**: `KEY_NUMERIC_0`
- **1**: `KEY_NUMERIC_1`
- **2**: `KEY_NUMERIC_2`
- **3**: `KEY_NUMERIC_3`
- **4**: `KEY_NUMERIC_4`
- **5**: `KEY_NUMERIC_5`
- **6**: `KEY_NUMERIC_6`
- **7**: `KEY_NUMERIC_7`
- **8**: `KEY_NUMERIC_8`
- **9**: `KEY_NUMERIC_9`
- dot: `KEY_DOT`
- enter (This is not the same as the select key): `KEY_ENTER`
- clear: `KEY_CLEAR`
- next-favorite: `KEY_NEXT_FAVORITE`
- channel-up: `KEY_CHANNELUP`
- channel-down: `KEY_CHANNELDOWN`
- previous-channel: `KEY_PREVIOUS`
- sound-select: `KEY_SOUND`
- input-select: *unmapped*
- display-information: `KEY_INFO`
- help: `KEY_HELP`
- page-up: `KEY_PAGEUP`
- page-down: `KEY_PAGEDOWN`
- power: `KEY_POWER`
- volume-up: `KEY_VOLUMEUP`
- volume-down: `KEY_VOLUMEDOWN`
- mute: `KEY_MUTE`
- play: `KEY_PLAYCD`
- stop: `KEY_STOPCD`
- pause: `KEY_PAUSECD`
- record: `KEY_RECORD`
- rewind: `KEY_REWIND`
- fast-forward: `KEY_FASTFORWARD`
- eject: `KEY_EJECTCD`
- skip-forward: `KEY_NEXTSONG`
- skip-backward: `KEY_PREVIOUSSONG`
- stop-record: `KEY_STOP_RECORD`
- pause-record: `KEY_PAUSE_RECORD`
- angle: `KEY_ANGLE`
- sub-picture: *unmapped*
- video-on-demand: `KEY_VOD`
- electronic-program-guide: `KEY_EPG`
- timer-programming: `KEY_TIME`
- initial-configuration: `KEY_CONFIG`
- select-broadcast-type [UI Broadcast Type]: *unmapped*
- select-sound-presentation [UI Sound Presentation Control]: *unmapped*
- audio-description: `KEY_AUDIO_DESC`
- internet: `KEY_WWW`
- 3d-mode: `KEY_3D_MODE`
- play-function [Play Mode]: *unmapped*
- pause-play-function: *unmapped*
- record-function: *unmapped*
- pause-record-function: *unmapped*
- stop-function: *unmapped*
- mute-function: *unmapped*
- restore-volume-function: `KEY_UNMUTE`
- tune-function [Channel Identifier]: *unmapped*
- select-media-function [UI Function Media]: *unmapped*
- select-av-input-function [UI Function Select A/V Input]: *unmapped*
- select-audio-input-function [UI Function Select Audio Input]: *unmapped*
- power-toggle-function: `KEY_POWER`
- power-off-function: `KEY_SLEEP`
- power-on-function: `KEY_WAKEUP`
- **f1**, **blue**: `KEY_BLUE`
- **f2**, **red**: `KEY_RED`
- **f3**, **green**: `KEY_GREEN`
- **f4**, **yellow**: `KEY_YELLOW`
- **f5**: `KEY_F5`
- data: `KEY_DATA`

---

linux-cec is copyright © 2024 – 2026, Valve Software.
Excluding the linux-cec-sys crate, all crates are licensed under the LGPL 2.1 or newer. linux-cec-sys is licensed
under the BSD 3-clause license and incorporates code written by Cisco Systems, Inc. for the Linux kernel.
