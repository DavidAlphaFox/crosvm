// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::CStr;

use arch::fdt::{
    begin_node, end_node, finish_fdt, generate_prop32, generate_prop64, property, property_cstring,
    property_null, property_string, property_u32, property_u64, start_fdt, Error,
};
use devices::PciInterruptPin;
use sys_util::{GuestAddress, GuestMemory};

// This is the start of DRAM in the physical address space.
use AARCH64_PHYS_MEM_START;

// These are GIC address-space location constants.
use AARCH64_GIC_CPUI_BASE;
use AARCH64_GIC_CPUI_SIZE;
use AARCH64_GIC_DIST_BASE;
use AARCH64_GIC_DIST_SIZE;

// These are RTC related constants
use devices::pl030::PL030_AMBA_ID;
use AARCH64_RTC_ADDR;
use AARCH64_RTC_IRQ;
use AARCH64_RTC_SIZE;

// These are serial device related constants.
use AARCH64_SERIAL_ADDR;
use AARCH64_SERIAL_IRQ;
use AARCH64_SERIAL_SIZE;
use AARCH64_SERIAL_SPEED;

// These are related to guest virtio devices.
use AARCH64_IRQ_BASE;
use AARCH64_MMIO_BASE;
use AARCH64_MMIO_SIZE;
use AARCH64_PCI_CFG_BASE;
use AARCH64_PCI_CFG_SIZE;

// This is an arbitrary number to specify the node for the GIC.
// If we had a more complex interrupt architecture, then we'd need an enum for
// these.
const PHANDLE_GIC: u32 = 1;

// These are specified by the Linux GIC bindings
const GIC_FDT_IRQ_NUM_CELLS: u32 = 3;
const GIC_FDT_IRQ_TYPE_SPI: u32 = 0;
const GIC_FDT_IRQ_TYPE_PPI: u32 = 1;
const GIC_FDT_IRQ_PPI_CPU_SHIFT: u32 = 8;
const GIC_FDT_IRQ_PPI_CPU_MASK: u32 = (0xff << GIC_FDT_IRQ_PPI_CPU_SHIFT);
const IRQ_TYPE_EDGE_RISING: u32 = 0x00000001;
const IRQ_TYPE_LEVEL_HIGH: u32 = 0x00000004;
const IRQ_TYPE_LEVEL_LOW: u32 = 0x00000008;

fn create_memory_node(fdt: &mut Vec<u8>, guest_mem: &GuestMemory) -> Result<(), Box<Error>> {
    let mem_size = guest_mem.memory_size();
    let mem_reg_prop = generate_prop64(&[AARCH64_PHYS_MEM_START, mem_size]);

    begin_node(fdt, "memory")?;
    property_string(fdt, "device_type", "memory")?;
    property(fdt, "reg", &mem_reg_prop)?;
    end_node(fdt)?;
    Ok(())
}

fn create_cpu_nodes(fdt: &mut Vec<u8>, num_cpus: u32) -> Result<(), Box<Error>> {
    begin_node(fdt, "cpus")?;
    property_u32(fdt, "#address-cells", 0x1)?;
    property_u32(fdt, "#size-cells", 0x0)?;

    for cpu_id in 0..num_cpus {
        let cpu_name = format!("cpu@{:x}", cpu_id);
        begin_node(fdt, &cpu_name)?;
        property_string(fdt, "device_type", "cpu")?;
        property_string(fdt, "compatible", "arm,arm-v8")?;
        if num_cpus > 1 {
            property_string(fdt, "enable-method", "psci")?;
        }
        property_u32(fdt, "reg", cpu_id)?;
        end_node(fdt)?;
    }
    end_node(fdt)?;
    Ok(())
}

fn create_gic_node(fdt: &mut Vec<u8>) -> Result<(), Box<Error>> {
    let gic_reg_prop = generate_prop64(&[
        AARCH64_GIC_DIST_BASE,
        AARCH64_GIC_DIST_SIZE,
        AARCH64_GIC_CPUI_BASE,
        AARCH64_GIC_CPUI_SIZE,
    ]);

    begin_node(fdt, "intc")?;
    property_string(fdt, "compatible", "arm,cortex-a15-gic")?;
    property_u32(fdt, "#interrupt-cells", GIC_FDT_IRQ_NUM_CELLS)?;
    property_null(fdt, "interrupt-controller")?;
    property(fdt, "reg", &gic_reg_prop)?;
    property_u32(fdt, "phandle", PHANDLE_GIC)?;
    property_u32(fdt, "#address-cells", 2)?;
    property_u32(fdt, "#size-cells", 2)?;
    end_node(fdt)?;

    Ok(())
}

fn create_timer_node(fdt: &mut Vec<u8>, num_cpus: u32) -> Result<(), Box<Error>> {
    // These are fixed interrupt numbers for the timer device.
    let irqs = [13, 14, 11, 10];
    let compatible = "arm,armv8-timer";
    let cpu_mask: u32 =
        (((1 << num_cpus) - 1) << GIC_FDT_IRQ_PPI_CPU_SHIFT) & GIC_FDT_IRQ_PPI_CPU_MASK;

    let mut timer_reg_cells = Vec::new();
    for &irq in irqs.iter() {
        timer_reg_cells.push(GIC_FDT_IRQ_TYPE_PPI);
        timer_reg_cells.push(irq);
        timer_reg_cells.push(cpu_mask | IRQ_TYPE_LEVEL_LOW);
    }
    let timer_reg_prop = generate_prop32(timer_reg_cells.as_slice());

    begin_node(fdt, "timer")?;
    property_string(fdt, "compatible", compatible)?;
    property(fdt, "interrupts", &timer_reg_prop)?;
    property_null(fdt, "always-on")?;
    end_node(fdt)?;

    Ok(())
}

fn create_serial_node(fdt: &mut Vec<u8>) -> Result<(), Box<Error>> {
    let serial_reg_prop = generate_prop64(&[AARCH64_SERIAL_ADDR, AARCH64_SERIAL_SIZE]);
    let irq = generate_prop32(&[
        GIC_FDT_IRQ_TYPE_SPI,
        AARCH64_SERIAL_IRQ,
        IRQ_TYPE_EDGE_RISING,
    ]);

    begin_node(fdt, "U6_16550A@3f8")?;
    property_string(fdt, "compatible", "ns16550a")?;
    property(fdt, "reg", &serial_reg_prop)?;
    property_u32(fdt, "clock-frequency", AARCH64_SERIAL_SPEED)?;
    property(fdt, "interrupts", &irq)?;
    end_node(fdt)?;

    Ok(())
}

// TODO(sonnyrao) -- check to see if host kernel supports PSCI 0_2
fn create_psci_node(fdt: &mut Vec<u8>) -> Result<(), Box<Error>> {
    let compatible = "arm,psci-0.2";
    begin_node(fdt, "psci")?;
    property_string(fdt, "compatible", compatible)?;
    // Only support aarch64 guest
    property_string(fdt, "method", "hvc")?;
    // These constants are from PSCI
    property_u32(fdt, "cpu_suspend", 0xc4000001)?;
    property_u32(fdt, "cpu_off", 0x84000002)?;
    property_u32(fdt, "cpu_on", 0xc4000003)?;
    property_u32(fdt, "migrate", 0xc4000005)?;
    end_node(fdt)?;

    Ok(())
}

fn create_chosen_node(fdt: &mut Vec<u8>, cmdline: &CStr) -> Result<(), Box<Error>> {
    begin_node(fdt, "chosen")?;
    property_u32(fdt, "linux,pci-probe-only", 1)?;
    property_cstring(fdt, "bootargs", cmdline)?;
    property_u64(fdt, "kaslr", 0)?;
    end_node(fdt)?;

    Ok(())
}

fn create_pci_nodes(
    fdt: &mut Vec<u8>,
    pci_irqs: Vec<(u32, PciInterruptPin)>,
) -> Result<(), Box<Error>> {
    // Add devicetree nodes describing a PCI generic host controller.
    // See Documentation/devicetree/bindings/pci/host-generic-pci.txt in the kernel
    // and "PCI Bus Binding to IEEE Std 1275-1994".
    let ranges = generate_prop32(&[
        // bus address (ss = 01: 32-bit memory space)
        0x2000000,
        (AARCH64_MMIO_BASE >> 32) as u32,
        AARCH64_MMIO_BASE as u32,
        // CPU address
        (AARCH64_MMIO_BASE >> 32) as u32,
        AARCH64_MMIO_BASE as u32,
        // size
        (AARCH64_MMIO_SIZE >> 32) as u32,
        AARCH64_MMIO_SIZE as u32,
    ]);
    let bus_range = generate_prop32(&[0, 0]); // Only bus 0
    let reg = generate_prop64(&[AARCH64_PCI_CFG_BASE, AARCH64_PCI_CFG_SIZE]);

    let mut interrupts: Vec<u32> = Vec::new();
    let mut masks: Vec<u32> = Vec::new();

    for (i, pci_irq) in pci_irqs.iter().enumerate() {
        // PCI_DEVICE(3)
        interrupts.push((pci_irq.0 + 1) << 11);
        interrupts.push(0);
        interrupts.push(0);

        // INT#(1)
        interrupts.push(pci_irq.1.to_mask() + 1);

        // CONTROLLER(PHANDLE)
        interrupts.push(PHANDLE_GIC);
        interrupts.push(0);
        interrupts.push(0);

        // CONTROLLER_DATA(3)
        interrupts.push(GIC_FDT_IRQ_TYPE_SPI);
        interrupts.push(AARCH64_IRQ_BASE + i as u32);
        interrupts.push(IRQ_TYPE_LEVEL_HIGH);

        // PCI_DEVICE(3)
        masks.push(0xf800); // bits 11..15 (device)
        masks.push(0);
        masks.push(0);

        // INT#(1)
        masks.push(0x7); // allow INTA#-INTD# (1 | 2 | 3 | 4)
    }

    let interrupt_map = generate_prop32(&interrupts);
    let interrupt_map_mask = generate_prop32(&masks);

    begin_node(fdt, "pci")?;
    property_string(fdt, "compatible", "pci-host-cam-generic")?;
    property_string(fdt, "device_type", "pci")?;
    property(fdt, "ranges", &ranges)?;
    property(fdt, "bus-range", &bus_range)?;
    property_u32(fdt, "#address-cells", 3)?;
    property_u32(fdt, "#size-cells", 2)?;
    property(fdt, "reg", &reg)?;
    property_u32(fdt, "#interrupt-cells", 1)?;
    property(fdt, "interrupt-map", &interrupt_map)?;
    property(fdt, "interrupt-map-mask", &interrupt_map_mask)?;
    end_node(fdt)?;

    Ok(())
}

fn create_rtc_node(fdt: &mut Vec<u8>) -> Result<(), Box<Error>> {
    // the kernel driver for pl030 really really wants a clock node
    // associated with an AMBA device or it will fail to probe, so we
    // need to make up a clock node to associate with the pl030 rtc
    // node and an associated handle with a unique phandle value.
    const CLK_PHANDLE: u32 = 24;
    begin_node(fdt, "pclk@3M")?;
    property_u32(fdt, "#clock-cells", 0)?;
    property_string(fdt, "compatible", "fixed-clock")?;
    property_u32(fdt, "clock-frequency", 3141592)?;
    property_u32(fdt, "phandle", CLK_PHANDLE)?;
    end_node(fdt)?;

    let rtc_name = format!("rtc@{:x}", AARCH64_RTC_ADDR);
    let reg = generate_prop64(&[AARCH64_RTC_ADDR, AARCH64_RTC_SIZE]);
    let irq = generate_prop32(&[GIC_FDT_IRQ_TYPE_SPI, AARCH64_RTC_IRQ, IRQ_TYPE_LEVEL_HIGH]);

    begin_node(fdt, &rtc_name)?;
    property_string(fdt, "compatible", "arm,primecell")?;
    property_u32(fdt, "arm,primecell-periphid", PL030_AMBA_ID)?;
    property(fdt, "reg", &reg)?;
    property(fdt, "interrupts", &irq)?;
    property_u32(fdt, "clocks", CLK_PHANDLE)?;
    property_string(fdt, "clock-names", "apb_pclk")?;
    end_node(fdt)?;
    Ok(())
}

/// Creates a flattened device tree containing all of the parameters for the
/// kernel and loads it into the guest memory at the specified offset.
///
/// # Arguments
///
/// * `fdt_max_size` - The amount of space reserved for the device tree
/// * `guest_mem` - The guest memory object
/// * `pci_irqs` - List of PCI device number to PCI interrupt pin mappings
/// * `num_cpus` - Number of virtual CPUs the guest will have
/// * `fdt_load_offset` - The offset into physical memory for the device tree
/// * `cmdline` - The kernel commandline
pub fn create_fdt(
    fdt_max_size: usize,
    guest_mem: &GuestMemory,
    pci_irqs: Vec<(u32, PciInterruptPin)>,
    num_cpus: u32,
    fdt_load_offset: u64,
    cmdline: &CStr,
) -> Result<(), Box<Error>> {
    let mut fdt = vec![0; fdt_max_size];
    start_fdt(&mut fdt, fdt_max_size)?;

    // The whole thing is put into one giant node with some top level properties
    begin_node(&mut fdt, "")?;
    property_u32(&mut fdt, "interrupt-parent", PHANDLE_GIC)?;
    property_string(&mut fdt, "compatible", "linux,dummy-virt")?;
    property_u32(&mut fdt, "#address-cells", 0x2)?;
    property_u32(&mut fdt, "#size-cells", 0x2)?;

    create_chosen_node(&mut fdt, cmdline)?;
    create_memory_node(&mut fdt, guest_mem)?;
    create_cpu_nodes(&mut fdt, num_cpus)?;
    create_gic_node(&mut fdt)?;
    create_timer_node(&mut fdt, num_cpus)?;
    create_serial_node(&mut fdt)?;
    create_psci_node(&mut fdt)?;
    create_pci_nodes(&mut fdt, pci_irqs)?;
    create_rtc_node(&mut fdt)?;
    // End giant node
    end_node(&mut fdt)?;

    // Allocate another buffer so we can format and then write fdt to guest
    let mut fdt_final = vec![0; fdt_max_size];
    finish_fdt(&mut fdt, &mut fdt_final, fdt_max_size)?;

    let fdt_address = GuestAddress(AARCH64_PHYS_MEM_START + fdt_load_offset);
    let written = guest_mem
        .write_at_addr(fdt_final.as_slice(), fdt_address)
        .map_err(|_| Error::FdtGuestMemoryWriteError)?;
    if written < fdt_max_size {
        return Err(Box::new(Error::FdtGuestMemoryWriteError));
    }
    Ok(())
}
