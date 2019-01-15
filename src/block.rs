// Copyright © 2019 Intel Corporation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::mem;

const QUEUE_SIZE: usize = 16;

#[repr(C)]
#[repr(align(16))]
#[derive(Default)]
/// A virtio qeueue entry descriptor
struct Desc {
    addr: u64,
    length: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
#[repr(align(2))]
#[derive(Default)]
/// The virtio available ring
struct AvailRing {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
}

#[repr(C)]
#[repr(align(4))]
#[derive(Default)]
/// The virtio used ring
struct UsedRing {
    flags: u16,
    idx: u16,
    ring: [UsedElem; QUEUE_SIZE],
}

#[repr(C)]
#[derive(Default)]
/// A single element in the used ring
struct UsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
#[repr(align(64))]
#[derive(Default)]
/// Device driver for virtio block over MMIO
pub struct VirtioMMIOBlockDevice {
    descriptors: [Desc; QUEUE_SIZE],

    region: mem::MemoryRegion,

    avail: AvailRing,
    used: UsedRing,
    next_head: usize,
}

pub enum Error {
    VirtioMagicInvalid,
    VirtioVersionInvalid,
    VirtioUnsupportedDevice,
    VirtioLegacyOnly,
    VirtioFeatureNegotiationFailed,
    VirtioQueueTooSmall,
    BlockIOError,
    BlockNotSupported,
}

#[repr(C)]
/// Header used for virtio block requests
struct BlockRequestHeader {
    request: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
/// Footer used for virtio block requests
struct BlockRequestFooter {
    status: u8,
}

pub trait SectorRead {
    /// Read a single sector (512 bytes) from the block device. `data` must be 
    /// exactly 512 bytes long.
    fn read(&mut self, sector: u64, data: &mut [u8]) -> Result<(), Error>;
}

impl VirtioMMIOBlockDevice {
    pub fn new(base: u64) -> VirtioMMIOBlockDevice {
        VirtioMMIOBlockDevice {
            region: mem::MemoryRegion::new(base, 4096),
            ..VirtioMMIOBlockDevice::default()
        }
    }

    fn get_status(&self) -> u32 {
        self.region.io_read_u32(0x70)
    }

    fn set_status(&self, value: u32) {
        self.region.io_write_u32(0x70, value);
    }

    fn add_status(&self, value: u32) {
        self.set_status(self.get_status() | value);
    }

    pub fn init(&self) -> Result<(), Error> {
        const VIRTIO_MAGIC: u32 = 0x74726976;
        const VIRTIO_VERSION: u32 = 0x2;
        const VIRTIO_SUBSYSTEM_BLOCK: u32 = 0x2;
        const VIRTIO_F_VERSION_1: u64 = 1 << 32;

        const VIRTIO_STATUS_RESET: u32 = 0;
        const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
        const VIRTIO_STATUS_DRIVER: u32 = 2;
        const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
        const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
        const VIRTIO_STATUS_FAILED: u32 = 128;

        if self.region.io_read_u32(0x000) != VIRTIO_MAGIC {
            return Err(Error::VirtioMagicInvalid);
        }

        if self.region.io_read_u32(0x004) != VIRTIO_VERSION {
            return Err(Error::VirtioVersionInvalid);
        }

        if self.region.io_read_u32(0x008) != VIRTIO_SUBSYSTEM_BLOCK {
            return Err(Error::VirtioUnsupportedDevice);
        }

        // Reset device
        self.set_status(VIRTIO_STATUS_RESET);

        // Acknowledge
        self.add_status(VIRTIO_STATUS_ACKNOWLEDGE);

        // And advertise driver
        self.add_status(VIRTIO_STATUS_DRIVER);

        // Request device features
        self.region.io_write_u32(0x014, 0);
        let mut device_features: u64 = self.region.io_read_u32(0x010) as u64;
        self.region.io_write_u32(0x014, 1);
        device_features |= (self.region.io_read_u32(0x010) as u64) << 32;

        if device_features & VIRTIO_F_VERSION_1 != VIRTIO_F_VERSION_1 {
            self.add_status(VIRTIO_STATUS_FAILED);
            return Err(Error::VirtioLegacyOnly);
        }

        // Report driver features
        self.region.io_write_u32(0x024, 0);
        let driver_features = device_features;
        self.region.io_write_u32(0x020, driver_features as u32);
        self.region.io_write_u32(0x024, 1);
        self.region
            .io_write_u32(0x020, (driver_features >> 32) as u32);

        self.add_status(VIRTIO_STATUS_FEATURES_OK);
        if self.get_status() & VIRTIO_STATUS_FEATURES_OK != VIRTIO_STATUS_FEATURES_OK {
            self.add_status(VIRTIO_STATUS_FAILED);
            return Err(Error::VirtioFeatureNegotiationFailed);
        }

        // Program queues
        self.region.io_write_u32(0x030, 0);
        let max_queue = self.region.io_read_u32(0x034);

        // Hardcoded queue size to QUEUE_SIZE at the moment
        if max_queue < QUEUE_SIZE as u32 {
            self.add_status(VIRTIO_STATUS_FAILED);
            return Err(Error::VirtioQueueTooSmall);
        }
        self.region.io_write_u32(0x038, QUEUE_SIZE as u32);

        // Update all queue parts
        let addr = self.descriptors.as_ptr() as u64;
        self.region.io_write_u32(0x080, addr as u32);
        self.region.io_write_u32(0x084, (addr >> 32) as u32);

        let addr = (&self.avail as *const _) as u64;
        self.region.io_write_u32(0x090, addr as u32);
        self.region.io_write_u32(0x094, (addr >> 32) as u32);

        let addr = (&self.used as *const _) as u64;
        self.region.io_write_u32(0x0a0, addr as u32);
        self.region.io_write_u32(0x0a4, (addr >> 32) as u32);

        // Confirm queue
        self.region.io_write_u32(0x044, 0x1);

        // Report driver ready
        self.add_status(VIRTIO_STATUS_DRIVER_OK);

        Ok(())
    }
}

impl SectorRead for VirtioMMIOBlockDevice {
    fn read(&mut self, sector: u64, data: &mut [u8]) -> Result<(), Error> {
        assert_eq!(512, data.len());

        const VIRTQ_DESC_F_NEXT: u16 = 1;
        const VIRTQ_DESC_F_WRITE: u16 = 2;

        const VIRTIO_BLK_S_OK: u8 = 0;
        const VIRTIO_BLK_S_IOERR: u8 = 1;
        const VIRTIO_BLK_S_UNSUPP: u8 = 2;

        let header = BlockRequestHeader {
            request: 0,
            reserved: 0,
            sector: sector,
        };

        let footer = BlockRequestFooter { status: 0 };

        let mut d = &mut self.descriptors[self.next_head];
        let next_desc = (self.next_head + 1) % QUEUE_SIZE;
        d.addr = (&header as *const _) as u64;
        d.length = core::mem::size_of::<BlockRequestHeader>() as u32;
        d.flags = VIRTQ_DESC_F_NEXT;
        d.next = next_desc as u16;

        let mut d = &mut self.descriptors[next_desc];
        let next_desc = (next_desc + 1) % QUEUE_SIZE;
        d.addr = data.as_ptr() as u64;
        d.length = core::mem::size_of::<[u8; 512]>() as u32;
        d.flags = VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE;
        d.next = next_desc as u16;

        let mut d = &mut self.descriptors[next_desc];
        d.addr = (&footer as *const _) as u64;
        d.length = core::mem::size_of::<BlockRequestFooter>() as u32;
        d.flags = VIRTQ_DESC_F_WRITE;
        d.next = 0;

        // Update ring to point to head of chain. Fence. Then update idx
        self.avail.ring[(self.avail.idx % QUEUE_SIZE as u16) as usize] = self.next_head as u16;
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

        self.avail.idx = self.avail.idx.wrapping_add(1);

        // Next free descriptor to use
        self.next_head = (next_desc + 1) % QUEUE_SIZE;

        // Notify queue has been updated
        self.region.io_write_u32(0x50, 0);

        // Check for the completion of the request
        while self.used.idx != self.avail.idx {
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        }

        match footer.status {
            VIRTIO_BLK_S_OK => Ok(()),
            VIRTIO_BLK_S_IOERR => Err(Error::BlockIOError),
            VIRTIO_BLK_S_UNSUPP => Err(Error::BlockNotSupported),
            _ => Err(Error::BlockNotSupported),
        }
    }
}
