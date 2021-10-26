use super::*;
use crate::header::VirtIOHeader;
use crate::queue::VirtQueue;
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use bitflags::*;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use log::*;
use spin::Mutex;
use volatile::Volatile;

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
    blkinfos: Box<[BlkInfo]>,
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
        let queue_size = header.max_queue_size();
        let queue = VirtQueue::new(header, 0, queue_size as u16)?;
        header.finish_init();

        Ok(VirtIOBlk {
            capacity: config.capacity.read() as usize,
            inner: Mutex::new(VirtIoBlkInner {
                header,
                queue,
                blkinfos: {
                    let mut vec = Vec::<BlkInfo>::with_capacity(queue_size as usize);
                    vec.resize_with(queue_size as usize, || NULLINFO);
                    vec.into_boxed_slice()
                },
            }),
        })
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(self: &Arc<Self>) -> bool {
        let mut inner = self.inner.lock();
        inner.header.ack_interrupt()
    }

    /// Handle virtio blk intrupt.
    pub fn handle_irq(self: &Arc<Self>) -> Result {
        let mut inner = self.inner.lock();
        if !inner.queue.can_pop() {
            return Err(Error::NotReady);
        }
        while inner.queue.can_pop() {
            let (idx, _) = inner.queue.pop_used()?;
            if let Some(waker) = inner.blkinfos[idx as usize].waker.take() {
                waker.wake();
            }
        }
        Ok(())
    }

    /// Read a block.
    pub fn read_block(
        self: &Arc<Self>,
        block_id: usize,
        buf: &mut [u8],
    ) -> Pin<Box<BlkFuture<'a>>> {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut inner = self.inner.lock();
        let mut future = Box::pin(BlkFuture::new(Arc::clone(&self)));
        match inner
            .queue
            .add(&[req.as_buf()], &[buf, future.resp.as_buf_mut()])
        {
            Ok(n) => {
                future.head = n;
                inner.header.notify(0);
            }
            Err(e) => future.err = Some(e),
        }
        future
    }

    /// Write a block.
    pub fn write_block(self: &Arc<Self>, block_id: usize, buf: &[u8]) -> Pin<Box<BlkFuture<'a>>> {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut inner = self.inner.lock();
        let mut future = Box::pin(BlkFuture::new(Arc::clone(&self)));
        match inner
            .queue
            .add(&[req.as_buf(), buf], &[future.resp.as_buf_mut()])
        {
            Ok(n) => {
                future.head = n;
                inner.header.notify(0);
            }
            Err(e) => future.err = Some(e),
        }
        inner.header.notify(0);
        future
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

#[repr(C)]
#[derive(Debug)]
struct BlkInfo {
    waker: Option<Waker>,
}

const NULLINFO: BlkInfo = BlkInfo::new();

impl BlkInfo {
    const fn new() -> Self {
        BlkInfo { waker: None }
    }
}

pub struct BlkFuture<'a> {
    resp: BlkResp,
    head: u16,
    driver: Arc<VirtIOBlk<'a>>,
    err: Option<Error>,
}

impl<'a> BlkFuture<'a> {
    pub fn new(driver: Arc<VirtIOBlk<'a>>) -> Self {
        Self {
            resp: BlkResp::default(),
            head: u16::MAX,
            driver: Arc::clone(&driver),
            err: None,
        }
    }
}

impl Future for BlkFuture<'_> {
    type Output = Result;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if let Some(e) = self.err {
            return Poll::Ready(Err(e));
        }
        let mut driver = self.driver.inner.lock();
        match unsafe { core::ptr::read_volatile(&self.resp.status) } {
            RespStatus::Ok => Poll::Ready(Ok(())),
            RespStatus::_NotReady => {
                driver.blkinfos[self.head as usize].waker = Some(cx.waker().clone());
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
