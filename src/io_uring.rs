use crossbeam_deque::{Injector, Steal};
use io_uring::opcode::{
    Accept, Connect, EpollCtl, Fsync, OpenAt, Read, Readv, Recv, RecvMsg, Send, SendMsg, Timeout,
    TimeoutRemove, TimeoutUpdate, Write, Writev,
};
use io_uring::squeue::Entry;
use io_uring::types::{epoll_event, Fd, Timespec};
use io_uring::{CompletionQueue, IoUring, Probe};
use libc::{c_int, c_void, iovec, msghdr, off_t, size_t, sockaddr, socklen_t};
use once_cell::sync::Lazy;
use std::fmt::{Debug, Formatter};
use std::io::{Error, ErrorKind};
use std::time::Duration;

pub struct IoUringOperator {
    io_uring: IoUring,
    backlog: Injector<&'static Entry>,
}

impl Debug for IoUringOperator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoUringSelector")
            .field("backlog", &self.backlog)
            .finish()
    }
}

static PROBE: Lazy<Probe> = Lazy::new(|| {
    let mut probe = Probe::new();
    if let Ok(io_uring) = IoUring::new(2) {
        if let Ok(()) = io_uring.submitter().register_probe(&mut probe) {
            return probe;
        }
    }
    panic!("probe init failed !")
});

// check https://www.rustwiki.org.cn/en/reference/introduction.html for help information
macro_rules! support {
    ( $opcode:ident ) => {
        once_cell::sync::Lazy::new(|| {
            if crate::version::support_io_uring() {
                return PROBE.is_supported(io_uring::opcode::$opcode::CODE);
            }
            false
        })
    };
}

static SUPPORT_OPENAT: Lazy<bool> = support!(OpenAt);

static SUPPORT_FSYNC: Lazy<bool> = support!(Fsync);

static SUPPORT_TIMEOUT_ADD: Lazy<bool> = support!(Timeout);

static SUPPORT_TIMEOUT_UPDATE: Lazy<bool> = support!(TimeoutUpdate);

static SUPPORT_TIMEOUT_REMOVE: Lazy<bool> = support!(TimeoutRemove);

static SUPPORT_EPOLL_CTL: Lazy<bool> = support!(EpollCtl);

static SUPPORT_ACCEPT: Lazy<bool> = support!(Accept);

static SUPPORT_CONNECT: Lazy<bool> = support!(Connect);

static SUPPORT_RECV: Lazy<bool> = support!(Recv);

static SUPPORT_READ: Lazy<bool> = support!(Read);

static SUPPORT_READV: Lazy<bool> = support!(Readv);

static SUPPORT_RECVMSG: Lazy<bool> = support!(RecvMsg);

static SUPPORT_SEND: Lazy<bool> = support!(Send);

static SUPPORT_WRITE: Lazy<bool> = support!(Write);

static SUPPORT_WRITEV: Lazy<bool> = support!(Writev);

static SUPPORT_SENDMSG: Lazy<bool> = support!(SendMsg);

impl IoUringOperator {
    pub fn new(cpu: u32) -> std::io::Result<Self> {
        Ok(IoUringOperator {
            io_uring: IoUring::builder()
                .setup_sqpoll(1000)
                .setup_sqpoll_cpu(cpu)
                .build(1024)?,
            backlog: Injector::new(),
        })
    }

    fn push_sq(&self, entry: Entry) {
        let entry = Box::leak(Box::new(entry));
        if unsafe { self.io_uring.submission_shared().push(entry).is_err() } {
            self.backlog.push(entry);
        }
    }

    /// select impl

    pub fn select(&self, timeout: Option<Duration>) -> std::io::Result<(usize, CompletionQueue)> {
        if crate::version::support_io_uring() {
            self.timeout_add(0, timeout)?;
            let r = self.io_uring.submit_and_wait(1);
            let mut cq = unsafe { self.io_uring.completion_shared() };
            cq.sync();

            // clean backlog
            let mut sq = unsafe { self.io_uring.submission_shared() };
            loop {
                if sq.is_full() {
                    match self.io_uring.submit() {
                        Ok(_) => (),
                        Err(err) => {
                            if err.raw_os_error() == Some(libc::EBUSY) {
                                break;
                            }
                            return Err(err);
                        }
                    }
                }
                sq.sync();

                loop {
                    match self.backlog.steal() {
                        Steal::Success(sqe) => {
                            if unsafe { sq.push(sqe).is_err() } {
                                self.backlog.push(sqe);
                                break;
                            }
                        }
                        Steal::Retry => continue,
                        Steal::Empty => break,
                    }
                }
            }
            return r.map(|count| (count, cq));
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    /// epoll

    pub fn epoll_ctl(
        &self,
        user_data: usize,
        epfd: c_int,
        op: c_int,
        fd: c_int,
        event: *mut libc::epoll_event,
    ) -> std::io::Result<()> {
        if *SUPPORT_EPOLL_CTL {
            let entry = EpollCtl::new(
                Fd(epfd),
                Fd(fd),
                op,
                event as *const _ as u64 as *const epoll_event,
            )
            .build()
            .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    /// timeout

    pub fn timeout_add(&self, user_data: usize, timeout: Option<Duration>) -> std::io::Result<()> {
        if let Some(duration) = timeout {
            if *SUPPORT_TIMEOUT_ADD {
                let timeout = Timespec::new()
                    .sec(duration.as_secs())
                    .nsec(duration.subsec_nanos());
                let entry = Timeout::new(&timeout).build().user_data(user_data as u64);
                self.push_sq(entry);
                return Ok(());
            }
            return Err(Error::new(ErrorKind::Unsupported, "unsupported"));
        }
        Ok(())
    }

    pub fn timeout_update(
        &self,
        user_data: usize,
        timeout: Option<Duration>,
    ) -> std::io::Result<()> {
        if let Some(duration) = timeout {
            if *SUPPORT_TIMEOUT_UPDATE {
                let timeout = Timespec::new()
                    .sec(duration.as_secs())
                    .nsec(duration.subsec_nanos());
                let entry = TimeoutUpdate::new(user_data as u64, &timeout)
                    .build()
                    .user_data(user_data as u64);
                self.push_sq(entry);
                return Ok(());
            }
            return Err(Error::new(ErrorKind::Unsupported, "unsupported"));
        }
        self.timeout_remove(user_data)
    }

    pub fn timeout_remove(&self, user_data: usize) -> std::io::Result<()> {
        if *SUPPORT_TIMEOUT_REMOVE {
            let entry = TimeoutRemove::new(user_data as u64).build();
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    /// file IO

    pub fn openat(
        &self,
        user_data: usize,
        dir_fd: c_int,
        pathname: *const libc::c_char,
        flags: c_int,
        mode: libc::mode_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_OPENAT {
            let entry = OpenAt::new(Fd(dir_fd), pathname)
                .flags(flags)
                .mode(mode)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn fsync(&self, user_data: usize, fd: c_int) -> std::io::Result<()> {
        if *SUPPORT_FSYNC {
            let entry = Fsync::new(Fd(fd)).build().user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    /// socket

    pub fn accept(
        &self,
        user_data: usize,
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_ACCEPT {
            let entry = Accept::new(Fd(socket), address, address_len)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn accept4(
        &self,
        user_data: usize,
        fd: c_int,
        addr: *mut sockaddr,
        len: *mut socklen_t,
        flg: c_int,
    ) -> std::io::Result<()> {
        if *SUPPORT_ACCEPT {
            let entry = Accept::new(Fd(fd), addr, len)
                .flags(flg)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn connect(
        &self,
        user_data: usize,
        socket: c_int,
        address: *const sockaddr,
        len: socklen_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_CONNECT {
            let entry = Connect::new(Fd(socket), address, len)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    /// read

    pub fn recv(
        &self,
        user_data: usize,
        socket: c_int,
        buf: *mut c_void,
        len: size_t,
        flags: c_int,
    ) -> std::io::Result<()> {
        if *SUPPORT_RECV {
            let entry = Recv::new(Fd(socket), buf.cast::<u8>(), len as u32)
                .flags(flags)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn read(
        &self,
        user_data: usize,
        fd: c_int,
        buf: *mut c_void,
        count: size_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_READ {
            let entry = Read::new(Fd(fd), buf.cast::<u8>(), count as u32)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn pread(
        &self,
        user_data: usize,
        fd: c_int,
        buf: *mut c_void,
        count: size_t,
        offset: off_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_READ {
            let entry = Read::new(Fd(fd), buf.cast::<u8>(), count as u32)
                .offset(offset as u64)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn readv(
        &self,
        user_data: usize,
        fd: c_int,
        iov: *const iovec,
        iovcnt: c_int,
    ) -> std::io::Result<()> {
        if *SUPPORT_READV {
            let entry = Readv::new(Fd(fd), iov, iovcnt as u32)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn preadv(
        &self,
        user_data: usize,
        fd: c_int,
        iov: *const iovec,
        iovcnt: c_int,
        offset: off_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_READV {
            let entry = Readv::new(Fd(fd), iov, iovcnt as u32)
                .offset(offset as u64)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn recvmsg(
        &self,
        user_data: usize,
        fd: c_int,
        msg: *mut msghdr,
        flags: c_int,
    ) -> std::io::Result<()> {
        if *SUPPORT_RECVMSG {
            let entry = RecvMsg::new(Fd(fd), msg)
                .flags(flags as u32)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    /// write

    pub fn send(
        &self,
        user_data: usize,
        socket: c_int,
        buf: *const c_void,
        len: size_t,
        flags: c_int,
    ) -> std::io::Result<()> {
        if *SUPPORT_SEND {
            let entry = Send::new(Fd(socket), buf.cast::<u8>(), len as u32)
                .flags(flags)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn write(
        &self,
        user_data: usize,
        fd: c_int,
        buf: *const c_void,
        count: size_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_WRITE {
            let entry = Write::new(Fd(fd), buf.cast::<u8>(), count as u32)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn pwrite(
        &self,
        user_data: usize,
        fd: c_int,
        buf: *const c_void,
        count: size_t,
        offset: off_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_WRITE {
            let entry = Write::new(Fd(fd), buf.cast::<u8>(), count as u32)
                .offset(offset as u64)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn writev(
        &self,
        user_data: usize,
        fd: c_int,
        iov: *const iovec,
        iovcnt: c_int,
    ) -> std::io::Result<()> {
        if *SUPPORT_WRITEV {
            let entry = Writev::new(Fd(fd), iov, iovcnt as u32)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn pwritev(
        &self,
        user_data: usize,
        fd: c_int,
        iov: *const iovec,
        iovcnt: c_int,
        offset: off_t,
    ) -> std::io::Result<()> {
        if *SUPPORT_WRITEV {
            let entry = Writev::new(Fd(fd), iov, iovcnt as u32)
                .offset(offset as u64)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }

    pub fn sendmsg(
        &self,
        user_data: usize,
        fd: c_int,
        msg: *const msghdr,
        flags: c_int,
    ) -> std::io::Result<()> {
        if *SUPPORT_SENDMSG {
            let entry = SendMsg::new(Fd(fd), msg)
                .flags(flags as u32)
                .build()
                .user_data(user_data as u64);
            self.push_sq(entry);
            return Ok(());
        }
        Err(Error::new(ErrorKind::Unsupported, "unsupported"))
    }
}