/*
 * Copyright © 2026 Valve Software
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::libc::TIOCSETD;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::stat::Mode;
use nix::sys::termios::{
    cfsetispeed, cfsetospeed, tcgetattr, tcsetattr, BaudRate, ControlFlags, InputFlags, LocalFlags,
    OutputFlags, SetArg, SpecialCharacterIndices,
};
use nix::unistd::read;
use nix::{ioctl_write_ptr, ioctl_write_ptr_bad};
use std::collections::HashMap;
use std::ffi::{c_int, c_ulong};
use std::io::{Error, ErrorKind};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{spawn, JoinHandle};
use udev::{Device, Enumerator, EventType, MonitorBuilder};

#[derive(Debug, PartialEq, Copy, Clone, Hash)]
#[repr(u8)]
pub enum SerioType {
    Pulse8 = 0x40,
    RainShadow = 0x41,
}

#[derive(Debug)]
pub struct CecTty {
    fd: OwnedFd,
    ty: SerioType,
}

#[derive(Debug, Clone)]
struct CancellationToken {
    value: Arc<AtomicBool>,
}

ioctl_write_ptr!(spiocstype, b'q', 1, c_ulong);
ioctl_write_ptr_bad!(tiocsetd, TIOCSETD, c_int);

const N_MOUSE: c_int = 2;

impl CecTty {
    pub fn new(path: impl AsRef<Path>, ty: SerioType) -> Result<CecTty, Error> {
        let fd = open(
            path.as_ref(),
            OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            Mode::empty(),
        )?;
        Ok(CecTty { fd, ty })
    }

    pub fn configure(&self) -> Result<(), Error> {
        let mut termios = tcgetattr(&self.fd)?;
        cfsetispeed(&mut termios, BaudRate::B9600)?;
        cfsetospeed(&mut termios, BaudRate::B9600)?;
        termios.input_flags = InputFlags::IGNBRK | InputFlags::IGNPAR;
        termios.output_flags = OutputFlags::empty();
        termios.control_flags = ControlFlags::CLOCAL | ControlFlags::CREAD | ControlFlags::CS8;
        termios.local_flags = LocalFlags::empty();
        termios.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        termios.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;

        tcsetattr(&self.fd, SetArg::TCSANOW, &termios)?;

        unsafe { tiocsetd(self.fd.as_raw_fd(), &N_MOUSE)? };
        unsafe { spiocstype(self.fd.as_raw_fd(), &(self.ty as c_ulong))? };

        Ok(())
    }

    pub fn poll(&self) -> Result<(), Error> {
        let mut byte = [0u8];
        match read(&self.fd, &mut byte) {
            Ok(_) => (),
            Err(e) => {
                println!("Read {e}");
            }
        }

        Ok(())
    }
}

struct CecTtyThread {
    token: CancellationToken,
    _handle: JoinHandle<()>,
}

pub(crate) struct CecTtyPoller {
    mappings: HashMap<(&'static str, &'static str, &'static str), SerioType>,
    threads: HashMap<PathBuf, CecTtyThread>,
}

fn tty_thread(
    path: impl AsRef<Path>,
    ty: SerioType,
    token: CancellationToken,
) -> Result<(), Error> {
    let tty = CecTty::new(path, ty)?;
    tty.configure()?;
    loop {
        match tty.poll() {
            Ok(_) => (),
            Err(e) if e.kind() == ErrorKind::Interrupted => (),
            Err(e) => return Err(e),
        }
        if token.is_cancelled() {
            return Ok(());
        }
    }
}

impl CecTtyPoller {
    pub(crate) fn run() -> Result<(), Error> {
        let tty_monitor = MonitorBuilder::new()?.match_subsystem("tty")?.listen()?;
        let mut tty_iter = tty_monitor.iter();

        let mappings = HashMap::<(&str, &str, &str), SerioType>::from_iter([
            (("usb", "2548", "1001"), SerioType::Pulse8),
            (("usb", "2548", "1002"), SerioType::Pulse8),
            (("usb", "04d8", "ff59"), SerioType::RainShadow),
        ]);

        let mut poller = CecTtyPoller {
            mappings,
            threads: HashMap::new(),
        };

        poller.populate()?;

        loop {
            let tty_fd = PollFd::new(tty_monitor.as_fd(), PollFlags::POLLERR | PollFlags::POLLIN);
            let mut fds = [tty_fd];
            let res = poll(&mut fds, PollTimeout::NONE);
            match res {
                Ok(n) if n < 1 => continue,
                Ok(_) => (),
                Err(Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
            match fds[0].revents() {
                Some(e) if e.contains(PollFlags::POLLERR) => break,
                Some(_) => {
                    for ev in tty_iter.by_ref() {
                        let dev = ev.device();
                        match ev.event_type() {
                            EventType::Add => poller.add_dev(dev),
                            EventType::Remove => poller.remove_dev(dev),
                            _ => (),
                        }
                    }
                }
                None => continue,
            }
        }
        Ok(())
    }

    fn dev_type(&self, dev: &Device) -> Option<SerioType> {
        let bus = dev.property_value("ID_BUS")?.to_str()?;
        let id = dev.property_value("ID_VENDOR_ID")?.to_str()?;
        let model = dev.property_value("ID_MODEL_ID")?.to_str()?;
        self.mappings.get(&(bus, id, model)).cloned()
    }

    fn populate(&mut self) -> Result<(), Error> {
        let mut enumerator = Enumerator::new()?;
        enumerator.match_subsystem("tty")?;
        for dev in enumerator.scan_devices()? {
            self.add_dev(dev);
        }
        Ok(())
    }

    fn add_dev(&mut self, dev: Device) {
        let Some(ty) = self.dev_type(&dev) else {
            return;
        };
        let Some(node) = dev.devnode() else {
            return;
        };
        if self.threads.contains_key(node) {
            return;
        }
        println!("Adding device {}", node.display());
        let token = CancellationToken::new();
        let node = node.to_path_buf();
        let handle = {
            let token = token.clone();
            let node = node.clone();
            spawn(move || {
                if let Err(err) = tty_thread(node, ty, token) {
                    println!("Error bridging tty: {err}");
                }
            })
        };
        let thread = CecTtyThread {
            token,
            _handle: handle,
        };
        self.threads.insert(node.to_path_buf(), thread);
    }

    fn remove_dev(&mut self, dev: Device) {
        let Some(node) = dev.devnode() else {
            return;
        };
        let Some(mut thread) = self.threads.remove(node) else {
            return;
        };
        println!("Removing device {}", node.display());
        thread.token.cancel();
    }
}

impl CancellationToken {
    fn new() -> CancellationToken {
        CancellationToken {
            value: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.value.load(Ordering::Relaxed)
    }

    fn cancel(&mut self) {
        self.value.store(true, Ordering::Relaxed);
    }
}

fn main() -> Result<(), Error> {
    println!("Starting up");
    CecTtyPoller::run()
}
