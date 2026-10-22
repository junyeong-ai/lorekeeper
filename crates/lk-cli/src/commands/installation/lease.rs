//! Cooperative claims on persistent installation lock files.

use std::fs::File;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rustix::fs::{Mode as FileMode, OFlags};
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

const MAX_CLAIMS: usize = 16;
const HANDOFF_MARKER: u8 = 1;
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Shared,
    Exclusive,
}

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub path: PathBuf,
    pub mode: Mode,
}

#[derive(Debug)]
pub(super) struct Lease {
    claims: Vec<(File, Mode)>,
}

impl Lease {
    pub fn acquire(requests: &[Request]) -> io::Result<Self> {
        let requests = ordered_requests(requests)?;
        let mut claims = Vec::with_capacity(requests.len());
        for request in requests {
            let file = open_lock(&request.path, true)?;
            lock(&file, request.mode)?;
            claims.push((file, request.mode));
        }
        Ok(Self { claims })
    }

    /// Keep claims alive when a foreground workflow replaces its process image.
    pub fn inherit(&self) -> io::Result<()> {
        for (file, _) in &self.claims {
            let flags = fcntl_getfd(file)?;
            fcntl_setfd(file, flags & !FdFlags::CLOEXEC)?;
        }
        Ok(())
    }

    /// Give the new binary the same exclusive open-file descriptions through stdin.
    pub fn handoff(&self) -> io::Result<Stdio> {
        if self.claims.iter().any(|(_, mode)| *mode != Mode::Exclusive) {
            return Err(invalid("installation handoff requires exclusive claims"));
        }
        let (sender, receiver) = UnixStream::pair()?;
        sender.set_write_timeout(Some(HANDOFF_TIMEOUT))?;
        let descriptors: Vec<_> = self.claims.iter().map(|(file, _)| file.as_fd()).collect();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_CLAIMS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        if !control.push(SendAncillaryMessage::ScmRights(&descriptors)) {
            return Err(invalid("installation handoff exceeds its descriptor bound"));
        }
        let marker = [HANDOFF_MARKER];
        if sendmsg(
            &sender,
            &[IoSlice::new(&marker)],
            &mut control,
            SendFlags::empty(),
        )? != marker.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "installation handoff marker was not sent",
            ));
        }
        Ok(Stdio::from(std::os::fd::OwnedFd::from(receiver)))
    }

    pub fn receive(requests: &[Request]) -> io::Result<Self> {
        Self::receive_from(&io::stdin(), requests)
    }

    fn receive_from(socket: &impl AsFd, requests: &[Request]) -> io::Result<Self> {
        let requests = ordered_requests(requests)?;
        if requests
            .iter()
            .any(|request| request.mode != Mode::Exclusive)
        {
            return Err(invalid("installation handoff requires exclusive requests"));
        }
        let mut marker = [0; 2];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_CLAIMS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        // The sender queues the complete packet before giving this socket to the child.
        #[cfg(target_os = "linux")]
        let flags = RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = RecvFlags::DONTWAIT;
        let received = recvmsg(
            socket,
            &mut [IoSliceMut::new(&mut marker)],
            &mut control,
            flags,
        )
        .map_err(|error| context("receive installation descriptors", error))?;
        let mut files = Vec::new();
        let mut unexpected = false;
        for message in control.drain() {
            match message {
                RecvAncillaryMessage::ScmRights(descriptors) => {
                    for descriptor in descriptors {
                        let file = File::from(descriptor);
                        let flags = fcntl_getfd(&file)
                            .map_err(|error| context("inspect received descriptor flags", error))?;
                        fcntl_setfd(&file, flags | FdFlags::CLOEXEC).map_err(|error| {
                            context("restrict received descriptor inheritance", error)
                        })?;
                        files.push(file);
                    }
                }
                _ => unexpected = true,
            }
        }
        if received
            .flags
            .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC)
            || received.bytes != 1
            || marker[0] != HANDOFF_MARKER
            || unexpected
            || files.len() != requests.len()
        {
            return Err(invalid("invalid installation descriptor handoff"));
        }
        let mut claims = Vec::with_capacity(files.len());
        for (file, request) in files.into_iter().zip(requests) {
            let expected = open_lock(&request.path, false)?.metadata()?;
            let actual = file.metadata()?;
            if !actual.is_file() || (actual.dev(), actual.ino()) != (expected.dev(), expected.ino())
            {
                return Err(invalid("installation handoff names a different lock file"));
            }
            // The inherited description shares the parent's lock. Never unlock or
            // downgrade it: either action would also change the parent's claim.
            lock(&file, Mode::Exclusive)
                .map_err(|error| context("claim received installation descriptor", error))?;
            claims.push((file, Mode::Exclusive));
        }
        Ok(Self { claims })
    }
}

fn ordered_requests(requests: &[Request]) -> io::Result<Vec<&Request>> {
    let mut ordered: Vec<_> = requests.iter().collect();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));
    for pair in ordered.windows(2) {
        if pair[0].path == pair[1].path && pair[0].mode != pair[1].mode {
            return Err(invalid("installation lock requested in conflicting modes"));
        }
    }
    ordered.dedup_by(|a, b| a.path == b.path);
    if ordered.is_empty() || ordered.len() > MAX_CLAIMS {
        return Err(invalid("installation requires between 1 and 16 lock files"));
    }
    if ordered.iter().any(|request| !request.path.is_absolute()) {
        return Err(invalid("installation lock paths must be absolute"));
    }
    Ok(ordered)
}

fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    if create {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match rustix::fs::open(
            path,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            FileMode::RUSR | FileMode::WUSR | FileMode::RGRP | FileMode::ROTH,
        ) {
            Ok(file) => drop(file),
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
    }
    // NONBLOCK prevents an occupied FIFO from hanging before its type is checked.
    let file = File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        FileMode::empty(),
    )?);
    if !file.metadata()?.is_file() {
        return Err(invalid("installation lock is not a regular file"));
    }
    Ok(file)
}

fn lock(file: &File, mode: Mode) -> io::Result<()> {
    let outcome = match mode {
        Mode::Shared => file.try_lock_shared(),
        Mode::Exclusive => file.try_lock(),
    };
    outcome.map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "installation resource is in use")
        }
        std::fs::TryLockError::Error(error) => error,
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn context(operation: &str, error: impl Into<io::Error>) -> io::Error {
    let error = error.into();
    io::Error::new(error.kind(), format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(path: &Path, mode: Mode) -> Request {
        Request {
            path: path.to_path_buf(),
            mode,
        }
    }

    #[test]
    fn readers_coexist_and_exclusive_claims_refuse_until_the_last_reader_exits() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("installation.lock");
        let first = Lease::acquire(&[request(&path, Mode::Shared)]).unwrap();
        let second = Lease::acquire(&[request(&path, Mode::Shared)]).unwrap();
        assert_eq!(
            Lease::acquire(&[request(&path, Mode::Exclusive)])
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        drop(first);
        assert!(Lease::acquire(&[request(&path, Mode::Exclusive)]).is_err());
        drop(second);
        let writer = Lease::acquire(&[request(&path, Mode::Exclusive)]).unwrap();
        assert!(Lease::acquire(&[request(&path, Mode::Shared)]).is_err());
        drop(writer);
        assert!(path.is_file());
        Lease::acquire(&[request(&path, Mode::Exclusive)]).unwrap();
    }

    #[test]
    fn duplicate_requests_share_one_claim_and_conflicting_modes_refuse() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("nested/installation.lock");
        let lease = Lease::acquire(&[
            request(&path, Mode::Exclusive),
            request(&path, Mode::Exclusive),
        ])
        .unwrap();
        assert_eq!(lease.claims.len(), 1);
        drop(lease);
        assert!(
            Lease::acquire(&[
                request(&path, Mode::Shared),
                request(&path, Mode::Exclusive),
            ])
            .is_err()
        );
    }

    #[test]
    fn lock_files_refuse_symlinks_and_nonregular_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("installation.lock");
        let target = temporary.path().join("target");
        std::fs::write(&target, "preserve").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(Lease::acquire(&[request(&path, Mode::Exclusive)]).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(Lease::acquire(&[request(&path, Mode::Exclusive)]).is_err());
    }

    #[test]
    fn handoff_validation_preserves_parent_claims() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("installation.lock");
        let other = temporary.path().join("other.lock");
        let lease = Lease::acquire(&[request(&path, Mode::Exclusive)]).unwrap();
        let other_lease = Lease::acquire(&[request(&other, Mode::Exclusive)]).unwrap();
        for (marker, descriptor, count, valid) in [
            (HANDOFF_MARKER, &lease.claims[0].0, 1, true),
            (0, &lease.claims[0].0, 1, false),
            (HANDOFF_MARKER, &other_lease.claims[0].0, 1, false),
            (HANDOFF_MARKER, &lease.claims[0].0, 2, false),
            (HANDOFF_MARKER, &lease.claims[0].0, MAX_CLAIMS + 1, false),
        ] {
            let (sender, receiver) = UnixStream::pair().unwrap();
            let descriptors = vec![descriptor.as_fd(); count];
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_CLAIMS + 1))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            assert!(control.push(SendAncillaryMessage::ScmRights(&descriptors)));
            assert_eq!(
                sendmsg(
                    &sender,
                    &[IoSlice::new(&[marker])],
                    &mut control,
                    SendFlags::empty(),
                )
                .unwrap(),
                1,
            );
            drop(sender);
            let received = Lease::receive_from(&receiver, &[request(&path, Mode::Exclusive)]);
            if valid {
                received.unwrap();
            } else {
                assert!(received.is_err());
            }
            assert_eq!(
                Lease::acquire(&[request(&path, Mode::Exclusive)])
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock,
            );
        }
    }

    #[test]
    fn handoff_refuses_an_empty_open_transport_without_waiting() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("installation.lock");
        let _lease = Lease::acquire(&[request(&path, Mode::Exclusive)]).unwrap();
        let (_sender, receiver) = UnixStream::pair().unwrap();
        let error = Lease::receive_from(&receiver, &[request(&path, Mode::Exclusive)]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn handoff_keeps_the_parent_claim_after_child_exit_and_rejects_forged_stdin() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("installation.lock");
        let lease = Lease::acquire(&[request(&path, Mode::Exclusive)]).unwrap();
        let name = format!(
            "{}::handoff_receiver",
            module_path!().split_once("::").unwrap().1,
        );
        for (input, expected) in [
            (lease.handoff().unwrap(), "receive"),
            (Stdio::null(), "reject"),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env("LORE_LEASE_TEST_PATH", &path)
                .env("LORE_LEASE_TEST_ACTION", expected)
                .stdin(input)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            assert_eq!(
                Lease::acquire(&[request(&path, Mode::Exclusive)])
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock,
            );
        }
        drop(lease);
        Lease::acquire(&[request(&path, Mode::Exclusive)]).unwrap();
    }

    #[test]
    fn handoff_receiver() {
        let Some(path) = std::env::var_os("LORE_LEASE_TEST_PATH") else {
            return;
        };
        let requests = [request(Path::new(&path), Mode::Exclusive)];
        let received = Lease::receive(&requests);
        if std::env::var("LORE_LEASE_TEST_ACTION").as_deref() == Ok("receive") {
            received.unwrap();
        } else {
            assert!(received.is_err());
        }
    }
}
