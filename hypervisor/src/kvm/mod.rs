// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
mod aarch64;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use aarch64::*;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod x86_64;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use x86_64::*;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::convert::TryFrom;
use std::ops::{Deref, DerefMut};
use std::os::raw::{c_char, c_ulong};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use libc::{open, EFAULT, EINVAL, EIO, ENOENT, ENOSPC, EOVERFLOW, O_CLOEXEC, O_RDWR};

use data_model::vec_with_array_field;
use kvm_sys::*;
use sync::Mutex;
use sys_util::{
    errno_result, ioctl, ioctl_with_ref, ioctl_with_val, AsRawDescriptor, Error, EventFd,
    FromRawDescriptor, GuestAddress, GuestMemory, MappedRegion, MmapError, RawDescriptor, Result,
    SafeDescriptor,
};

use crate::{
    ClockState, DeviceKind, Hypervisor, HypervisorCap, IrqRoute, IrqSource, RunnableVcpu, Vcpu,
    VcpuExit, Vm, VmCap,
};

// Wrapper around KVM_SET_USER_MEMORY_REGION ioctl, which creates, modifies, or deletes a mapping
// from guest physical to host user pages.
//
// Safe when the guest regions are guaranteed not to overlap.
unsafe fn set_user_memory_region(
    descriptor: &SafeDescriptor,
    slot: u32,
    read_only: bool,
    log_dirty_pages: bool,
    guest_addr: u64,
    memory_size: u64,
    userspace_addr: *mut u8,
) -> Result<()> {
    let mut flags = if read_only { KVM_MEM_READONLY } else { 0 };
    if log_dirty_pages {
        flags |= KVM_MEM_LOG_DIRTY_PAGES;
    }
    let region = kvm_userspace_memory_region {
        slot,
        flags,
        guest_phys_addr: guest_addr,
        memory_size,
        userspace_addr: userspace_addr as u64,
    };

    let ret = ioctl_with_ref(descriptor, KVM_SET_USER_MEMORY_REGION(), &region);
    if ret == 0 {
        Ok(())
    } else {
        errno_result()
    }
}

pub struct Kvm {
    kvm: SafeDescriptor,
}

type KvmCap = kvm::Cap;

impl Kvm {
    /// Opens `/dev/kvm/` and returns a Kvm object on success.
    pub fn new() -> Result<Kvm> {
        // Open calls are safe because we give a constant nul-terminated string and verify the
        // result.
        let ret = unsafe { open("/dev/kvm\0".as_ptr() as *const c_char, O_RDWR | O_CLOEXEC) };
        if ret < 0 {
            return errno_result();
        }
        // Safe because we verify that ret is valid and we own the fd.
        Ok(Kvm {
            kvm: unsafe { SafeDescriptor::from_raw_descriptor(ret) },
        })
    }
}

impl AsRawDescriptor for Kvm {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.kvm.as_raw_descriptor()
    }
}

impl AsRawFd for Kvm {
    fn as_raw_fd(&self) -> RawFd {
        self.kvm.as_raw_descriptor()
    }
}

impl Hypervisor for Kvm {
    fn check_capability(&self, cap: &HypervisorCap) -> bool {
        if let Ok(kvm_cap) = KvmCap::try_from(cap) {
            // this ioctl is safe because we know this kvm descriptor is valid,
            // and we are copying over the kvm capability (u32) as a c_ulong value.
            unsafe { ioctl_with_val(self, KVM_CHECK_EXTENSION(), kvm_cap as c_ulong) == 1 }
        } else {
            // this capability cannot be converted on this platform, so return false
            false
        }
    }
}

// Used to invert the order when stored in a max-heap.
#[derive(Copy, Clone, Eq, PartialEq)]
struct MemSlot(u32);

impl Ord for MemSlot {
    fn cmp(&self, other: &MemSlot) -> Ordering {
        // Notice the order is inverted so the lowest magnitude slot has the highest priority in a
        // max-heap.
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for MemSlot {
    fn partial_cmp(&self, other: &MemSlot) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A wrapper around creating and using a KVM VM.
pub struct KvmVm {
    vm: SafeDescriptor,
    guest_mem: GuestMemory,
    mem_regions: Arc<Mutex<BTreeMap<u32, Box<dyn MappedRegion>>>>,
    mem_slot_gaps: Arc<Mutex<BinaryHeap<MemSlot>>>,
}

impl KvmVm {
    /// Constructs a new `KvmVm` using the given `Kvm` instance.
    pub fn new(kvm: &Kvm, guest_mem: GuestMemory) -> Result<KvmVm> {
        // Safe because we know kvm is a real kvm fd as this module is the only one that can make
        // Kvm objects.
        let ret = unsafe { ioctl(kvm, KVM_CREATE_VM()) };
        if ret < 0 {
            return errno_result();
        }
        // Safe because we verify that ret is valid and we own the fd.
        let vm_descriptor = unsafe { SafeDescriptor::from_raw_descriptor(ret) };
        guest_mem.with_regions(|index, guest_addr, size, host_addr, _| {
            unsafe {
                // Safe because the guest regions are guaranteed not to overlap.
                set_user_memory_region(
                    &vm_descriptor,
                    index as u32,
                    false,
                    false,
                    guest_addr.offset(),
                    size as u64,
                    host_addr as *mut u8,
                )
            }
        })?;
        // TODO(colindr/srichman): add default IRQ routes in IrqChip constructor or configure_vm
        Ok(KvmVm {
            vm: vm_descriptor,
            guest_mem,
            mem_regions: Arc::new(Mutex::new(BTreeMap::new())),
            mem_slot_gaps: Arc::new(Mutex::new(BinaryHeap::new())),
        })
    }

    fn create_kvm_vcpu(&self, _id: usize) -> Result<KvmVcpu> {
        Ok(KvmVcpu {})
    }

    /// Crates an in kernel interrupt controller.
    ///
    /// See the documentation on the KVM_CREATE_IRQCHIP ioctl.
    pub fn create_irq_chip(&self) -> Result<()> {
        // Safe because we know that our file is a VM fd and we verify the return result.
        let ret = unsafe { ioctl(self, KVM_CREATE_IRQCHIP()) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }
    /// Sets the level on the given irq to 1 if `active` is true, and 0 otherwise.
    pub fn set_irq_line(&self, irq: u32, active: bool) -> Result<()> {
        let mut irq_level = kvm_irq_level::default();
        irq_level.__bindgen_anon_1.irq = irq;
        irq_level.level = if active { 1 } else { 0 };

        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_IRQ_LINE(), &irq_level) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    /// Registers an event that will, when signalled, trigger the `gsi` irq, and `resample_evt`
    /// ( when not None ) will be triggered when the irqchip is resampled.
    pub fn register_irqfd(
        &self,
        gsi: u32,
        evt: &EventFd,
        resample_evt: Option<&EventFd>,
    ) -> Result<()> {
        let mut irqfd = kvm_irqfd {
            fd: evt.as_raw_fd() as u32,
            gsi,
            ..Default::default()
        };

        if let Some(r_evt) = resample_evt {
            irqfd.flags = KVM_IRQFD_FLAG_RESAMPLE;
            irqfd.resamplefd = r_evt.as_raw_fd() as u32;
        }

        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_IRQFD(), &irqfd) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    /// Unregisters an event that was previously registered with
    /// `register_irqfd`.
    ///
    /// The `evt` and `gsi` pair must be the same as the ones passed into
    /// `register_irqfd`.
    pub fn unregister_irqfd(&self, gsi: u32, evt: &EventFd) -> Result<()> {
        let irqfd = kvm_irqfd {
            fd: evt.as_raw_fd() as u32,
            gsi,
            flags: KVM_IRQFD_FLAG_DEASSIGN,
            ..Default::default()
        };
        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_IRQFD(), &irqfd) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    /// Sets the GSI routing table, replacing any table set with previous calls to
    /// `set_gsi_routing`.
    pub fn set_gsi_routing(&self, routes: &[IrqRoute]) -> Result<()> {
        let mut irq_routing =
            vec_with_array_field::<kvm_irq_routing, kvm_irq_routing_entry>(routes.len());
        irq_routing[0].nr = routes.len() as u32;

        // Safe because we ensured there is enough space in irq_routing to hold the number of
        // route entries.
        let irq_routes = unsafe { irq_routing[0].entries.as_mut_slice(routes.len()) };
        for (route, irq_route) in routes.iter().zip(irq_routes.iter_mut()) {
            *irq_route = kvm_irq_routing_entry::from(route);
        }

        let ret = unsafe { ioctl_with_ref(self, KVM_SET_GSI_ROUTING(), &irq_routing[0]) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }
}

impl Vm for KvmVm {
    fn try_clone(&self) -> Result<Self> {
        Ok(KvmVm {
            vm: self.vm.try_clone()?,
            guest_mem: self.guest_mem.clone(),
            mem_regions: self.mem_regions.clone(),
            mem_slot_gaps: self.mem_slot_gaps.clone(),
        })
    }

    fn check_capability(&self, c: VmCap) -> bool {
        if let Some(val) = self.check_capability_arch(c) {
            return val;
        }
        match c {
            VmCap::DirtyLog => true,
            VmCap::PvClock => false,
            VmCap::PvClockSuspend => self.check_raw_capability(KVM_CAP_KVMCLOCK_CTRL),
        }
    }

    fn check_raw_capability(&self, cap: u32) -> bool {
        // Safe because we know that our file is a KVM fd, and if the cap is invalid KVM assumes
        // it's an unavailable extension and returns 0.
        unsafe { ioctl_with_val(self, KVM_CHECK_EXTENSION(), cap as c_ulong) == 1 }
    }

    fn get_memory(&self) -> &GuestMemory {
        &self.guest_mem
    }

    fn add_memory_region(
        &mut self,
        guest_addr: GuestAddress,
        mem: Box<dyn MappedRegion>,
        read_only: bool,
        log_dirty_pages: bool,
    ) -> Result<u32> {
        let size = mem.size() as u64;
        let end_addr = guest_addr.checked_add(size).ok_or(Error::new(EOVERFLOW))?;
        if self.guest_mem.range_overlap(guest_addr, end_addr) {
            return Err(Error::new(ENOSPC));
        }
        let mut regions = self.mem_regions.lock();
        let mut gaps = self.mem_slot_gaps.lock();
        let slot = match gaps.pop() {
            Some(gap) => gap.0,
            None => (regions.len() + self.guest_mem.num_regions() as usize) as u32,
        };

        // Safe because we check that the given guest address is valid and has no overlaps. We also
        // know that the pointer and size are correct because the MemoryMapping interface ensures
        // this. We take ownership of the memory mapping so that it won't be unmapped until the slot
        // is removed.
        let res = unsafe {
            set_user_memory_region(
                &self.vm,
                slot,
                read_only,
                log_dirty_pages,
                guest_addr.offset() as u64,
                size,
                mem.as_ptr(),
            )
        };

        if let Err(e) = res {
            gaps.push(MemSlot(slot));
            return Err(e);
        }
        regions.insert(slot, mem);
        Ok(slot)
    }

    fn msync_memory_region(&mut self, slot: u32, offset: usize, size: usize) -> Result<()> {
        let mut regions = self.mem_regions.lock();
        let mem = regions.get_mut(&slot).ok_or(Error::new(ENOENT))?;

        mem.msync(offset, size).map_err(|err| match err {
            MmapError::InvalidAddress => Error::new(EFAULT),
            MmapError::NotPageAligned => Error::new(EINVAL),
            MmapError::SystemCallFailed(e) => e,
            _ => Error::new(EIO),
        })
    }

    fn remove_memory_region(&mut self, slot: u32) -> Result<()> {
        let mut regions = self.mem_regions.lock();
        if !regions.contains_key(&slot) {
            return Err(Error::new(ENOENT));
        }
        // Safe because the slot is checked against the list of memory slots.
        unsafe {
            set_user_memory_region(&self.vm, slot, false, false, 0, 0, std::ptr::null_mut())?;
        }
        self.mem_slot_gaps.lock().push(MemSlot(slot));
        regions.remove(&slot);
        Ok(())
    }

    fn create_device(&self, kind: DeviceKind) -> Result<SafeDescriptor> {
        let device = if let Some(dev) = self.get_device_params_arch(kind) {
            dev
        } else {
            match kind {
                DeviceKind::Vfio => kvm_create_device {
                    type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
                    fd: 0,
                    flags: 0,
                },

                // ARM has additional DeviceKinds, so it needs the catch-all pattern
                #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
                _ => return Err(Error::new(libc::ENXIO)),
            }
        };

        // Safe because we know that our file is a VM fd, we know the kernel will only write correct
        // amount of memory to our pointer, and we verify the return result.
        let ret = unsafe { sys_util::ioctl_with_ref(self, KVM_CREATE_DEVICE(), &device) };
        if ret == 0 {
            // Safe because we verify that ret is valid and we own the fd.
            Ok(unsafe { SafeDescriptor::from_raw_descriptor(device.fd as i32) })
        } else {
            errno_result()
        }
    }

    fn get_pvclock(&self) -> Result<ClockState> {
        self.get_pvclock_arch()
    }

    fn set_pvclock(&self, state: &ClockState) -> Result<()> {
        self.set_pvclock_arch(state)
    }
}

impl AsRawDescriptor for KvmVm {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.vm.as_raw_descriptor()
    }
}

impl AsRawFd for KvmVm {
    fn as_raw_fd(&self) -> RawFd {
        self.vm.as_raw_descriptor()
    }
}

/// A wrapper around creating and using a KVM Vcpu.
pub struct KvmVcpu {}

impl Vcpu for KvmVcpu {
    type Runnable = RunnableKvmVcpu;

    fn to_runnable(self) -> Result<Self::Runnable> {
        Ok(RunnableKvmVcpu {
            vcpu: self,
            phantom: Default::default(),
        })
    }

    fn request_interrupt_window(&self) -> Result<()> {
        Ok(())
    }
}

/// A KvmVcpu that has a thread and can be run.
pub struct RunnableKvmVcpu {
    vcpu: KvmVcpu,

    // vcpus must stay on the same thread once they start.
    // Add the PhantomData pointer to ensure RunnableKvmVcpu is not `Send`.
    phantom: std::marker::PhantomData<*mut u8>,
}

impl RunnableVcpu for RunnableKvmVcpu {
    type Vcpu = KvmVcpu;

    fn run(&self) -> Result<VcpuExit> {
        Ok(VcpuExit::Unknown)
    }
}

impl Deref for RunnableKvmVcpu {
    type Target = <Self as RunnableVcpu>::Vcpu;

    fn deref(&self) -> &Self::Target {
        &self.vcpu
    }
}

impl DerefMut for RunnableKvmVcpu {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.vcpu
    }
}

impl<'a> TryFrom<&'a HypervisorCap> for KvmCap {
    type Error = Error;

    fn try_from(cap: &'a HypervisorCap) -> Result<KvmCap> {
        match cap {
            HypervisorCap::ArmPmuV3 => Ok(KvmCap::ArmPmuV3),
            HypervisorCap::ImmediateExit => Ok(KvmCap::ImmediateExit),
            HypervisorCap::S390UserSigp => Ok(KvmCap::S390UserSigp),
            HypervisorCap::TscDeadlineTimer => Ok(KvmCap::TscDeadlineTimer),
            HypervisorCap::UserMemory => Ok(KvmCap::UserMemory),
        }
    }
}

impl From<&IrqRoute> for kvm_irq_routing_entry {
    fn from(item: &IrqRoute) -> Self {
        match &item.source {
            IrqSource::Irqchip { chip, pin } => kvm_irq_routing_entry {
                gsi: item.gsi,
                type_: KVM_IRQ_ROUTING_IRQCHIP,
                u: kvm_irq_routing_entry__bindgen_ty_1 {
                    irqchip: kvm_irq_routing_irqchip {
                        irqchip: chip_to_kvm_chip(*chip),
                        pin: *pin,
                    },
                },
                ..Default::default()
            },
            IrqSource::Msi { address, data } => kvm_irq_routing_entry {
                gsi: item.gsi,
                type_: KVM_IRQ_ROUTING_MSI,
                u: kvm_irq_routing_entry__bindgen_ty_1 {
                    msi: kvm_irq_routing_msi {
                        address_lo: *address as u32,
                        address_hi: (*address >> 32) as u32,
                        data: *data,
                        pad: 0,
                    },
                },
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::FromRawFd;
    use std::thread;
    use sys_util::{GuestAddress, MemoryMapping, MemoryMappingArena};

    #[test]
    fn new() {
        Kvm::new().unwrap();
    }

    #[test]
    fn check_capability() {
        let kvm = Kvm::new().unwrap();
        assert!(kvm.check_capability(&HypervisorCap::UserMemory));
        assert!(!kvm.check_capability(&HypervisorCap::S390UserSigp));
    }

    #[test]
    fn create_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        KvmVm::new(&kvm, gm).unwrap();
    }

    #[test]
    fn clone_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        vm.try_clone().unwrap();
    }

    #[test]
    fn send_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        thread::spawn(move || {
            let _vm = vm;
        })
        .join()
        .unwrap();
    }

    #[test]
    fn check_vm_capability() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        assert!(vm.check_raw_capability(KVM_CAP_USER_MEMORY));
        // I assume nobody is testing this on s390
        assert!(!vm.check_raw_capability(KVM_CAP_S390_USER_SIGP));
    }

    #[test]
    fn get_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let obj_addr = GuestAddress(0xf0);
        vm.get_memory().write_obj_at_addr(67u8, obj_addr).unwrap();
        let read_val: u8 = vm.get_memory().read_obj_from_addr(obj_addr).unwrap();
        assert_eq!(read_val, 67u8);
    }

    #[test]
    fn add_memory() {
        let kvm = Kvm::new().unwrap();
        let gm =
            GuestMemory::new(&[(GuestAddress(0), 0x1000), (GuestAddress(0x5000), 0x5000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x1000;
        let mem = MemoryMapping::new(mem_size).unwrap();
        vm.add_memory_region(GuestAddress(0x1000), Box::new(mem), false, false)
            .unwrap();
        let mem = MemoryMapping::new(mem_size).unwrap();
        vm.add_memory_region(GuestAddress(0x10000), Box::new(mem), false, false)
            .unwrap();
    }

    #[test]
    fn add_memory_ro() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x1000;
        let mem = MemoryMapping::new(mem_size).unwrap();
        vm.add_memory_region(GuestAddress(0x1000), Box::new(mem), true, false)
            .unwrap();
    }

    #[test]
    fn remove_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x1000;
        let mem = MemoryMapping::new(mem_size).unwrap();
        let slot = vm
            .add_memory_region(GuestAddress(0x1000), Box::new(mem), false, false)
            .unwrap();
        vm.remove_memory_region(slot).unwrap();
    }

    #[test]
    fn remove_invalid_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        assert!(vm.remove_memory_region(0).is_err());
    }

    #[test]
    fn overlap_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x2000;
        let mem = MemoryMapping::new(mem_size).unwrap();
        assert!(vm
            .add_memory_region(GuestAddress(0x2000), Box::new(mem), false, false)
            .is_err());
    }

    #[test]
    fn sync_memory() {
        let kvm = Kvm::new().unwrap();
        let gm =
            GuestMemory::new(&[(GuestAddress(0), 0x1000), (GuestAddress(0x5000), 0x5000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x1000;
        let mem = MemoryMappingArena::new(mem_size).unwrap();
        let slot = vm
            .add_memory_region(GuestAddress(0x1000), Box::new(mem), false, false)
            .unwrap();
        vm.msync_memory_region(slot, mem_size, 0).unwrap();
        assert!(vm.msync_memory_region(slot, mem_size + 1, 0).is_err());
        assert!(vm.msync_memory_region(slot + 1, mem_size, 0).is_err());
    }

    #[test]
    fn register_irqfd() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&vec![(GuestAddress(0), 0x10000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let evtfd1 = EventFd::new().unwrap();
        let evtfd2 = EventFd::new().unwrap();
        let evtfd3 = EventFd::new().unwrap();
        vm.create_irq_chip().unwrap();
        vm.register_irqfd(4, &evtfd1, None).unwrap();
        vm.register_irqfd(8, &evtfd2, None).unwrap();
        vm.register_irqfd(4, &evtfd3, None).unwrap();
        vm.register_irqfd(4, &evtfd3, None).unwrap_err();
    }

    #[test]
    fn unregister_irqfd() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&vec![(GuestAddress(0), 0x10000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let evtfd1 = EventFd::new().unwrap();
        let evtfd2 = EventFd::new().unwrap();
        let evtfd3 = EventFd::new().unwrap();
        vm.create_irq_chip().unwrap();
        vm.register_irqfd(4, &evtfd1, None).unwrap();
        vm.register_irqfd(8, &evtfd2, None).unwrap();
        vm.register_irqfd(4, &evtfd3, None).unwrap();
        vm.unregister_irqfd(4, &evtfd1).unwrap();
        vm.unregister_irqfd(8, &evtfd2).unwrap();
        vm.unregister_irqfd(4, &evtfd3).unwrap();
    }

    #[test]
    fn irqfd_resample() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&vec![(GuestAddress(0), 0x10000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let evtfd1 = EventFd::new().unwrap();
        let evtfd2 = EventFd::new().unwrap();
        vm.create_irq_chip().unwrap();
        vm.register_irqfd(4, &evtfd1, Some(&evtfd2)).unwrap();
        vm.unregister_irqfd(4, &evtfd1).unwrap();
        // Ensures the ioctl is actually reading the resamplefd.
        vm.register_irqfd(4, &evtfd1, Some(unsafe { &EventFd::from_raw_fd(-1) }))
            .unwrap_err();
    }
}
