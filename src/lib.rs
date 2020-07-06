#![allow(clippy::new_without_default)]
#![allow(clippy::comparison_chain)]
use std::collections::HashMap;
use std::io;
use std::io::prelude::*;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time;

#[allow(non_camel_case_types)]
type nfds_t = libc::c_ulong;

pub use events::Events;

pub mod events {
    /// Events that can be waited for.
    pub type Events = libc::c_short;

    /// The associated file is ready to be read.
    pub const READ: Events = POLLIN | POLLPRI;
    /// The associated file is ready to be written.
    pub const WRITE: Events = POLLOUT | libc::POLLWRBAND;

    /// The associated file is available for read operations.
    pub(super) const POLLIN: Events = libc::POLLIN;
    /// There is urgent data available for read operations.
    pub(super) const POLLPRI: Events = libc::POLLPRI;
    /// The associated file is available for write operations.
    pub(super) const POLLOUT: Events = libc::POLLOUT;
    /// Error condition happened on the associated file descriptor.
    /// `poll` will always wait for this event; it is not necessary to set it.
    pub(super) const POLLERR: Events = libc::POLLERR;
    /// Hang up happened on the associated file descriptor.
    /// `poll` will always wait for this event; it is not necessary to set it.
    pub(super) const POLLHUP: Events = libc::POLLHUP;
    /// The associated file is invalid.
    /// `poll` will always wait for this event; it is not necessary to set it.
    pub(super) const POLLNVAL: Events = libc::POLLNVAL;
}

#[derive(Debug)]
pub struct Event<'a> {
    /// The file is writable.
    pub writable: bool,
    /// The file is readable.
    pub readable: bool,
    /// The file has be disconnected.
    pub hangup: bool,
    /// An error has occured on the file.
    pub errored: bool,
    /// The underlying descriptor.
    pub descriptor: &'a mut Descriptor,
}

impl<'a> Event<'a> {
    pub fn stream<T: FromRawFd>(&self) -> T {
        unsafe { T::from_raw_fd(self.descriptor.fd) }
    }
}

impl<'a> From<&'a mut Descriptor> for Event<'a> {
    fn from(descriptor: &'a mut Descriptor) -> Self {
        let revents = descriptor.revents;

        Self {
            readable: revents & events::READ != 0,
            writable: revents & events::WRITE != 0,
            hangup: revents & events::POLLHUP != 0,
            errored: revents & (events::POLLERR | events::POLLNVAL) != 0,
            descriptor,
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct Descriptor {
    fd: RawFd,
    events: Events,
    revents: Events,
}

impl Descriptor {
    pub fn new(fd: RawFd, events: Events) -> Self {
        Self {
            fd,
            events,
            revents: 0,
        }
    }

    pub fn set(&mut self, events: Events) {
        self.events |= events;
    }

    pub fn unset(&mut self, events: Events) {
        self.events &= !events;
    }
}

pub struct Descriptors<K> {
    wakers: HashMap<usize, UnixStream>,
    index: Vec<K>,
    list: Vec<Descriptor>,
}

impl<K: Eq + Clone> Descriptors<K> {
    pub fn new() -> Self {
        let wakers = HashMap::new();

        Self {
            wakers,
            index: vec![],
            list: vec![],
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        let wakers = HashMap::new();

        Self {
            wakers,
            index: Vec::with_capacity(cap),
            list: Vec::with_capacity(cap),
        }
    }

    pub fn register(&mut self, key: K, fd: &impl AsRawFd, events: Events) {
        self.insert(key, Descriptor::new(fd.as_raw_fd(), events));
    }

    pub fn unregister(&mut self, key: &K) {
        if let Some(ix) = self.index.iter().position(|k| k == key) {
            self.index.swap_remove(ix);
            self.list.swap_remove(ix);
        }
    }

    pub fn set(&mut self, key: &K, events: Events) -> bool {
        if let Some(ix) = self.index.iter().position(|k| k == key) {
            self.list[ix].events = events;
            return true;
        }
        false
    }

    pub fn insert(&mut self, key: K, descriptor: Descriptor) {
        self.index.push(key);
        self.list.push(descriptor);
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut Descriptor> {
        if let Some(ix) = self.index.iter().position(|k| k == key) {
            return Some(&mut self.list[ix]);
        }
        None
    }
}

pub enum Wait<'a, K> {
    Timeout,
    Ready(Box<dyn Iterator<Item = (K, Event<'a>)> + 'a>),
}

impl<'a, K: 'a> Wait<'a, K> {
    pub fn iter(self) -> Box<dyn Iterator<Item = (K, Event<'a>)> + 'a> {
        match self {
            Self::Ready(iter) => iter,
            Self::Timeout => Box::new(std::iter::empty()),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Timeout)
    }
}

/// Wakers are used to wake up `poll`.
///
/// Unlike the `mio` crate, it's okay to drop wakers after they have been used.
///
pub struct Waker {
    writer: UnixStream,
}

impl Waker {
    /// Create a new `Waker`.
    ///
    /// # Examples
    ///
    /// Wake a `wait` call from another thread.
    ///
    /// ```
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     use std::thread;
    ///     use std::time::Duration;
    ///
    ///     use popol::{Descriptors, Waker};
    ///
    ///     const WAKER: &'static str = "waker";
    ///
    ///     let mut descriptors = Descriptors::new();
    ///     let mut waker = Waker::new(&mut descriptors, WAKER)?;
    ///
    ///     let handle = thread::spawn(move || {
    ///         thread::sleep(Duration::from_millis(160));
    ///
    ///         // Wake the queue on the other thread.
    ///         waker.wake().expect("waking shouldn't fail");
    ///     });
    ///
    ///     // Wait to be woken up by the other thread. Otherwise, time out.
    ///     let result = popol::wait(&mut descriptors, Duration::from_secs(1))?;
    ///
    ///     assert!(!result.is_empty(), "There should be at least one event selected");
    ///
    ///     let mut events = result.iter();
    ///     let (key, event) = events.next().unwrap();
    ///
    ///     assert!(key == WAKER, "The event is triggered by the waker");
    ///     assert!(event.readable, "The event is readable");
    ///     assert!(events.next().is_none(), "There was only one event");
    ///
    ///     handle.join().unwrap();
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn new<K: Eq + Clone>(descriptors: &mut Descriptors<K>, key: K) -> io::Result<Waker> {
        let (writer, reader) = UnixStream::pair()?;
        let fd = reader.as_raw_fd();

        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;

        descriptors.wakers.insert(descriptors.list.len(), reader);
        descriptors.insert(key, Descriptor::new(fd, events::READ));

        Ok(Waker { writer })
    }

    pub fn wake(&mut self) -> io::Result<()> {
        self.writer.write_all(&[0x1])?;

        Ok(())
    }

    /// Unblock the waker. This allows it to be re-awoken.
    ///
    /// TODO: Unclear under what conditions this `read` can fail.
    ///       Possibly if interrupted by a signal?
    /// TODO: Handle `ErrorKind::Interrupted`
    /// TODO: Handle zero-length read?
    /// TODO: Handle `ErrorKind::WouldBlock`
    fn snooze(reader: &mut UnixStream) -> io::Result<()> {
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf)?;

        Ok(())
    }
}

pub fn wait<'a, K: Eq + Clone>(
    fds: &'a mut Descriptors<K>,
    timeout: time::Duration,
) -> Result<Wait<'a, K>, io::Error> {
    let timeout = timeout.as_millis() as libc::c_int;

    let result = unsafe {
        libc::poll(
            fds.list.as_mut_ptr() as *mut libc::pollfd,
            fds.list.len() as nfds_t,
            timeout,
        )
    };

    if result == 0 {
        Ok(Wait::Timeout)
    } else if result > 0 {
        let wakers = &mut fds.wakers;

        Ok(Wait::Ready(Box::new(
            fds.index
                .iter()
                .zip(fds.list.iter_mut())
                .enumerate()
                .filter(|(_, (_, d))| d.revents != 0)
                .map(move |(i, (key, descriptor))| {
                    let revents = descriptor.revents;

                    if let Some(reader) = wakers.get_mut(&i) {
                        assert!(revents & events::READ != 0);
                        assert!(revents & events::WRITE == 0);

                        Waker::snooze(reader).unwrap();
                    }

                    (key.clone(), Event::from(descriptor))
                }),
        )))
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_readable() -> io::Result<()> {
        let (writer0, reader0) = UnixStream::pair()?;
        let (writer1, reader1) = UnixStream::pair()?;
        let (writer2, reader2) = UnixStream::pair()?;

        let mut descriptors = Descriptors::new();

        for reader in &[&reader0, &reader1, &reader2] {
            reader.set_nonblocking(true)?;
        }

        descriptors.register("reader0", &reader0, events::READ);
        descriptors.register("reader1", &reader1, events::READ);
        descriptors.register("reader2", &reader2, events::READ);

        {
            let result = wait(&mut descriptors, Duration::from_millis(1))?;
            assert!(result.is_empty());
        }

        let tests = &mut [
            (&writer0, &reader0, "reader0", 0x1 as u8),
            (&writer1, &reader1, "reader1", 0x2 as u8),
            (&writer2, &reader2, "reader2", 0x3 as u8),
        ];

        for (mut writer, mut reader, key, byte) in tests.iter_mut() {
            let mut buf = [0u8; 1];

            assert!(matches!(
                reader.read(&mut buf[..]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock
            ));

            writer.write(&[*byte])?;

            let result = wait(&mut descriptors, Duration::from_millis(1))?;
            assert!(!result.is_empty());

            let mut events = result.iter();
            let (k, event) = events.next().unwrap();

            assert_eq!(&k, key);
            assert!(event.readable && !event.writable && !event.errored && !event.hangup);
            assert!(events.next().is_none());

            assert_eq!(reader.read(&mut buf[..])?, 1);
            assert_eq!(&buf[..], &[*byte]);
        }
        Ok(())
    }

    #[test]
    fn test_threaded() -> io::Result<()> {
        let (writer0, reader0) = UnixStream::pair()?;
        let (writer1, reader1) = UnixStream::pair()?;
        let (writer2, reader2) = UnixStream::pair()?;

        let mut descriptors = Descriptors::new();
        let readers = &[&reader0, &reader1, &reader2];

        for reader in readers {
            reader.set_nonblocking(true)?;
        }

        descriptors.register("reader0", &reader0, events::READ);
        descriptors.register("reader1", &reader1, events::READ);
        descriptors.register("reader2", &reader2, events::READ);

        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(8));

            for writer in &mut [&writer1, &writer2, &writer0] {
                writer.write(&[1]).unwrap();
                writer.write(&[2]).unwrap();
            }
        });

        let mut closed = vec![];
        while closed.len() < readers.len() {
            let result = wait(&mut descriptors, Duration::from_millis(64))?;

            assert!(!result.is_empty());

            for (key, event) in result.iter() {
                assert!(event.readable);
                assert!(!event.writable);
                assert!(!event.errored);

                if event.hangup {
                    closed.push(key);
                    continue;
                }

                let mut buf = [0u8; 2];
                let mut reader = match key {
                    "reader0" => &reader0,
                    "reader1" => &reader1,
                    "reader2" => &reader2,
                    _ => unreachable!(),
                };
                let n = reader.read(&mut buf[..])?;

                assert_eq!(n, 2);
                assert_eq!(&buf[..], &[1, 2]);
            }
        }
        handle.join().unwrap();

        Ok(())
    }
}
