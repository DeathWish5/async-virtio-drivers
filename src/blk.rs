use super::*;
use crate::header::VirtIOHeader;
use crate::queue::VirtQueue;
use alloc::sync::Arc;
use bitflags::*;
use core::hint::spin_loop;
use core::slice;
use spin::Mutex;
#[cfg(feature = "async")]
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use log::*;
use volatile::Volatile;

const QUEUE_SIZE: usize = 16;

/// The virtio block device is a simple virtual block device (ie. disk).
///
/// Read and write requests (and other exotic requests) are placed in the queue,
/// and serviced (probably out of order) by the device except where noted.
pub struct VirtIOBlk<'a> {
    capacity: usize,
    inner: Mutex<VirtIoBlkInner<'a>>,
}

struct VirtIoBlkInner<'a> {
    header: &'static mut VirtIOHeader,
    queue: VirtQueue<'a>,
    #[cfg(feature = "async")]
    blkinfos: [BlkInfo; QUEUE_SIZE],
}

impl<'a> VirtIOBlk<'a> {
    /// Create a new VirtIO-Blk driver.
    pub fn new(header: &'static mut VirtIOHeader) -> Result<Self> {
        header.begin_init(|features| {
            let features = BlkFeature::from_bits_truncate(features);
            info!("device features: {:?}", features);
            // negotiate these flags only
            let supported_features = BlkFeature::empty();
            (features & supported_features).bits()
        });

        // read configuration space
        let config = unsafe { &mut *(header.config_space() as *mut BlkConfig) };
        info!("config: {:?}", config);
        info!(
            "found a block device of size {}KB",
            config.capacity.read() / 2
        );

        let queue = VirtQueue::new(header, 0, QUEUE_SIZE as u16)?;
        header.finish_init();

        Ok(VirtIOBlk {
            capacity: config.capacity.read() as usize,
            inner: Mutex::new(VirtIoBlkInner {
                header,
                queue,
                #[cfg(feature = "async")]
                blkinfos: [NULLINFO; QUEUE_SIZE],
            }),
        })
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(self: Arc<Self>) -> bool {
        let mut inner = self.inner.lock();
        inner.header.ack_interrupt()
    }

    #[cfg(feature = "async")]
    /// Handle virtio blk intrupt.
    pub fn handle_interrupt(self: Arc<Self>) -> Result {
        let mut inner = self.inner.lock();
        if !inner.queue.can_pop() {
            return Err(Error::NotReady);
        }
        while inner.queue.can_pop() {
            let (idx, _) = inner.queue.pop_used()?;
            match inner.blkinfos[idx as usize].waker.take() {
                Some(waker) => waker.wake(),
                None => return Err(Error::UnknownError), // should not happend
            }
        }
        Ok(())
    }

    #[cfg(not(feature = "async"))]
    /// Read a block.
    pub fn read_block(self: Arc<Self>, block_id: usize, buf: &mut [u8]) -> Result {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut inner = self.inner.lock();
        let mut resp = BlkResp::default();
        inner
            .queue
            .add(&[req.as_buf()], &[buf, resp.as_buf_mut()])?;
        inner.header.notify(0);
        while !inner.queue.can_pop() {
            spin_loop();
        }
        inner.queue.pop_used()?;
        match resp.status {
            RespStatus::Ok => Ok(()),
            _ => Err(Error::IoError),
        }
    }

    #[cfg(feature = "async")]
    /// Read a block.
    pub fn read_block(self: Arc<Self>, block_id: usize, buf: &mut [u8]) -> Result<BlkFuture<'a>> {
        assert_eq!(buf.len(), BLK_SIZE);
        Ok(BlkFuture {
            arg: BlkArg {
                req: BlkReq {
                    type_: ReqType::In,
                    reserved: 0,
                    sector: block_id as u64,
                },
                resp: BlkResp::default(),
                addr: buf.as_ptr() as usize,
                len: buf.len(),
            },
            driver: Arc::clone(&self),
            inner: Mutex::new(BlkFutureInner {
                head: 0,
                first: true,
            }),
        })
    }

    #[cfg(not(feature = "async"))]
    /// Write a block.
    pub fn write_block(self: Arc<Self>, block_id: usize, buf: &[u8]) -> Result {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut inner = self.inner.lock();
        let mut resp = BlkResp::default();
        inner
            .queue
            .add(&[req.as_buf(), buf], &[resp.as_buf_mut()])?;
        inner.header.notify(0);
        while !inner.queue.can_pop() {
            spin_loop();
        }
        inner.queue.pop_used()?;
        match resp.status {
            RespStatus::Ok => Ok(()),
            _ => Err(Error::IoError),
        }
    }

    #[cfg(feature = "async")]
    /// Write a block.
    pub fn write_block(self: Arc<Self>, block_id: usize, buf: &[u8]) -> Result<BlkFuture<'a>> {
        assert_eq!(buf.len(), BLK_SIZE);
        Ok(BlkFuture {
            arg: BlkArg {
                req: BlkReq {
                    type_: ReqType::Out,
                    reserved: 0,
                    sector: block_id as u64,
                },
                resp: BlkResp::default(),
                addr: buf.as_ptr() as usize,
                len: buf.len(),
            },
            driver: Arc::clone(&self),
            inner: Mutex::new(BlkFutureInner {
                head: 0,
                first: true,
            }),
        })
    }
}

#[repr(C)]
#[derive(Debug)]
struct BlkConfig {
    /// Number of 512 Bytes sectors
    capacity: Volatile<u64>,
    size_max: Volatile<u32>,
    seg_max: Volatile<u32>,
    cylinders: Volatile<u16>,
    heads: Volatile<u8>,
    sectors: Volatile<u8>,
    blk_size: Volatile<u32>,
    physical_block_exp: Volatile<u8>,
    alignment_offset: Volatile<u8>,
    min_io_size: Volatile<u16>,
    opt_io_size: Volatile<u32>,
    // ... ignored
}

#[repr(C)]
#[derive(Debug)]
struct BlkReq {
    type_: ReqType,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
#[derive(Debug)]
struct BlkResp {
    status: RespStatus,
}

#[repr(u32)]
#[derive(Debug, Eq, PartialEq)]
enum ReqType {
    In = 0,
    Out = 1,
    Flush = 4,
    Discard = 11,
    WriteZeroes = 13,
}

#[repr(u8)]
#[derive(Debug, Eq, PartialEq)]
enum RespStatus {
    Ok = 0,
    IoErr = 1,
    Unsupported = 2,
    _NotReady = 0xFF,
}

impl Default for BlkResp {
    fn default() -> Self {
        Self {
            status: RespStatus::_NotReady,
        }
    }
}

#[cfg(feature = "async")]
#[repr(C)]
#[derive(Debug)]
struct BlkInfo {
    waker: Option<Waker>,
}

#[cfg(feature = "async")]
const NULLINFO: BlkInfo = BlkInfo::new();

#[cfg(feature = "async")]
impl BlkInfo {
    const fn new() -> Self {
        BlkInfo { waker: None }
    }
}

#[cfg(feature = "async")]
struct BlkArg {
    req: BlkReq,
    resp: BlkResp,
    addr: usize,
    len: usize,
}

#[cfg(feature = "async")]
pub struct BlkFuture<'a> {
    arg: BlkArg,
    driver: Arc<VirtIOBlk<'a>>,
    inner: Mutex<BlkFutureInner>,
}

#[cfg(feature = "async")]
struct BlkFutureInner {
    head: u16,
    first: bool,
}

#[cfg(feature = "async")]
impl Future for BlkFuture<'_> {
    type Output = Result;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut inner = self.inner.lock();
        let mut driver = self.driver.inner.lock();
        if inner.first {
            let mut inner = self.inner.lock();
            if self.arg.req.type_ == ReqType::In {
                let buf = unsafe { slice::from_raw_parts_mut(self.arg.addr as *mut _, self.arg.len) };
                inner.head = driver.queue.add(
                    &[self.arg.req.as_buf()],
                    &[buf, self.arg.resp.as_buf_mut_unchecked()],
                )?; 
            } else {
                let buf = unsafe { slice::from_raw_parts(self.arg.addr as *const _, self.arg.len) } ;
                inner.head = driver.queue.add(
                    &[self.arg.req.as_buf(), buf],
                    &[self.arg.resp.as_buf_mut_unchecked()],
                )?;
            }
            driver.header.notify(0);
            inner.first = false;
            return Poll::Pending;
        }
        match self.arg.resp.status {
            RespStatus::Ok => Poll::Ready(Ok(())),
            RespStatus::_NotReady => {
                driver.blkinfos[inner.head as usize].waker = Some(cx.waker().clone());
                Poll::Pending
            }
            _ => Poll::Ready(Err(Error::IoError)),
        }
    }
}

const BLK_SIZE: usize = 512;

bitflags! {
    struct BlkFeature: u64 {
        /// Device supports request barriers. (legacy)
        const BARRIER       = 1 << 0;
        /// Maximum size of any single segment is in `size_max`.
        const SIZE_MAX      = 1 << 1;
        /// Maximum number of segments in a request is in `seg_max`.
        const SEG_MAX       = 1 << 2;
        /// Disk-style geometry specified in geometry.
        const GEOMETRY      = 1 << 4;
        /// Device is read-only.
        const RO            = 1 << 5;
        /// Block size of disk is in `blk_size`.
        const BLK_SIZE      = 1 << 6;
        /// Device supports scsi packet commands. (legacy)
        const SCSI          = 1 << 7;
        /// Cache flush command support.
        const FLUSH         = 1 << 9;
        /// Device exports information on optimal I/O alignment.
        const TOPOLOGY      = 1 << 10;
        /// Device can toggle its cache between writeback and writethrough modes.
        const CONFIG_WCE    = 1 << 11;
        /// Device can support discard command, maximum discard sectors size in
        /// `max_discard_sectors` and maximum discard segment number in
        /// `max_discard_seg`.
        const DISCARD       = 1 << 13;
        /// Device can support write zeroes command, maximum write zeroes sectors
        /// size in `max_write_zeroes_sectors` and maximum write zeroes segment
        /// number in `max_write_zeroes_seg`.
        const WRITE_ZEROES  = 1 << 14;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // the following since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

unsafe impl AsBuf for BlkReq {}
unsafe impl AsBuf for BlkResp {}
