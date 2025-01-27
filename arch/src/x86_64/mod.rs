// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

mod gdt;
pub mod interrupts;
pub mod layout;
mod mptable;
pub mod regs;

use std::mem;

use arch_gen::x86::bootparam::{boot_params, E820_RAM};
use memory_model::{DataInit, GuestAddress, GuestMemory};

// This is a workaround to the Rust enforcement specifying that any implementation of a foreign
// trait (in this case `DataInit`) where:
// *    the type that is implementing the trait is foreign or
// *    all of the parameters being passed to the trait (if there are any) are also foreign
// is prohibited.
#[derive(Copy, Clone)]
struct BootParamsWrapper(boot_params);

// It is safe to initialize BootParamsWrap which is a wrapper over `boot_params` (a series of ints).
unsafe impl DataInit for BootParamsWrapper {}

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Invalid e820 setup params.
    E820Configuration,
    /// Error writing MP table to memory.
    MpTableSetup(mptable::Error),
    /// The zero page extends past the end of guest_mem.
    ZeroPagePastRamEnd,
    /// Error writing the zero page of guest memory.
    ZeroPageSetup,
}

impl From<Error> for super::Error {
    fn from(e: Error) -> super::Error {
        super::Error::X86_64Setup(e)
    }
}

// Where BIOS/VGA magic would live on a real PC.
const EBDA_START: u64 = 0x9fc00;
const FIRST_ADDR_PAST_32BITS: usize = (1 << 32);
const MEM_32BIT_GAP_SIZE: usize = (768 << 20);

/// Returns a Vec of the valid memory addresses.
/// These should be used to configure the GuestMemory structure for the platform.
/// For x86_64 all addresses are valid from the start of the kernel except a
/// carve out at the end of 32bit address space.
pub fn arch_memory_regions(size: usize) -> Vec<(GuestAddress, usize)> {
    let memory_gap_start = GuestAddress(FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE);
    let memory_gap_end = GuestAddress(FIRST_ADDR_PAST_32BITS);
    let requested_memory_size = GuestAddress(size);
    let mut regions = Vec::new();

    // case1: guest memory fits before the gap
    if requested_memory_size <= memory_gap_start {
        regions.push((GuestAddress(0), size));
    // case2: guest memory extends beyond the gap
    } else {
        // push memory before the gap
        regions.push((GuestAddress(0), memory_gap_start.offset()));
        regions.push((
            memory_gap_end,
            requested_memory_size.offset_from(memory_gap_start),
        ));
    }

    regions
}

/// X86 specific memory hole/memory mapped devices/reserved area.
pub fn get_32bit_gap_start() -> usize {
    FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE
}

/// Returns the memory address where the kernel could be loaded.
pub fn get_kernel_start() -> usize {
    layout::HIMEM_START
}

/// Configures the system and should be called once per vm before starting vcpu threads.
///
/// # Arguments
///
/// * `guest_mem` - The memory to be used by the guest.
/// * `cmdline_addr` - Address in `guest_mem` where the kernel command line was loaded.
/// * `cmdline_size` - Size of the kernel command line in bytes including the null terminator.
/// * `num_cpus` - Number of virtual CPUs the guest will have.
pub fn configure_system(
    guest_mem: &GuestMemory,
    cmdline_addr: GuestAddress,
    cmdline_size: usize,
    num_cpus: u8,
    pvh_boot: bool,
) -> super::Result<()> {
    if pvh_boot {
        setup_pvh_boot(guest_mem, cmdline_addr, num_cpus)?;
        return Ok(());
    }
    const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
    const KERNEL_HDR_MAGIC: u32 = 0x5372_6448;
    const KERNEL_LOADER_OTHER: u8 = 0xff;
    const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000; // Must be non-zero.
    let first_addr_past_32bits = GuestAddress(FIRST_ADDR_PAST_32BITS);
    let end_32bit_gap_start = GuestAddress(get_32bit_gap_start());

    let himem_start = GuestAddress(layout::HIMEM_START);

    // Note that this puts the mptable at the last 1k of Linux's 640k base RAM
    mptable::setup_mptable(guest_mem, num_cpus).map_err(Error::MpTableSetup)?;

    let mut params: BootParamsWrapper = BootParamsWrapper(boot_params::default());

    params.0.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    params.0.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    params.0.hdr.header = KERNEL_HDR_MAGIC;
    params.0.hdr.cmd_line_ptr = cmdline_addr.offset() as u32;
    warn!("The address of the kernel CMDLINE copied into bootparams is {:#x?}", cmdline_addr.offset() as u32);
    params.0.hdr.cmdline_size = cmdline_size as u32;
    params.0.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;

    add_e820_entry(&mut params.0, 0, EBDA_START, E820_RAM)?;

    let mem_end = guest_mem.end_addr();
    if mem_end < end_32bit_gap_start {
        add_e820_entry(
            &mut params.0,
            himem_start.offset() as u64,
            mem_end.offset_from(himem_start) as u64,
            E820_RAM,
        )?;
    } else {
        add_e820_entry(
            &mut params.0,
            himem_start.offset() as u64,
            end_32bit_gap_start.offset_from(himem_start) as u64,
            E820_RAM,
        )?;
        if mem_end > first_addr_past_32bits {
            add_e820_entry(
                &mut params.0,
                first_addr_past_32bits.offset() as u64,
                mem_end.offset_from(first_addr_past_32bits) as u64,
                E820_RAM,
            )?;
        }
    }

    warn!("In CONFIGURE_SYSTEM, the number of E820 entries is {:#?}, and E820map is as follows:",
        params.0.e820_entries);

    for i in 0..params.0.e820_entries {
        warn!("i: {:?}, entry: {:#x?}", i, params.0.e820_map[i as usize]);
    }

    let zero_page_addr = GuestAddress(layout::ZERO_PAGE_START);
    guest_mem
        .checked_offset(zero_page_addr, mem::size_of::<boot_params>())
        .ok_or(Error::ZeroPagePastRamEnd)?;
    guest_mem
        .write_obj_at_addr(params, zero_page_addr)
        .map_err(|_| Error::ZeroPageSetup)?;

    Ok(())
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct hvm_start_info {
    pub magic: u32,
    pub version: u32,
    pub flags: u32,
    pub nr_modules: u32,
    pub modlist_paddr: u64,
    pub cmdline_paddr: u64,
    pub rsdp_paddr: u64,
    pub memmap_paddr: u64,
    pub memmap_entries: u32,
    pub reserved: u32,
}
impl Default for hvm_start_info {
    fn default() -> Self {
        unsafe { ::std::mem::zeroed() }
    }
}

#[derive(Debug, Copy, Clone)]
struct HvmStartInfoWrapper(hvm_start_info);

unsafe impl DataInit for HvmStartInfoWrapper {}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct hvm_memmap_table_entry {
    pub addr: u64,
    pub size: u64,
    pub type_: u32,
    pub reserved: u32,
}

impl Default for hvm_memmap_table_entry {
    fn default() -> Self {
        unsafe { ::std::mem::zeroed() }
    }
}

#[derive(Debug, Copy, Clone)]
struct HvmMemmapTableEntryWrapper(hvm_memmap_table_entry);

//struct HvmMemmapTableEntryWrapper(Vec<hvm_memmap_table_entry>);

unsafe impl DataInit for HvmMemmapTableEntryWrapper {}

//unsafe impl DataInit for std::vec::Vec<x86_64::hvm_memmap_table_entry> {}

fn setup_pvh_boot(
    guest_mem: &GuestMemory,
    cmdline_addr: GuestAddress,
    num_cpus: u8,
) -> super::Result<()> {

    // const PVH_START_INFO: usize = 0x6000;
    const MEMMAP_START: usize = 0x7000;
    const XEN_HVM_START_MAGIC_VALUE: u32 = 0x336ec578;

    let himem_start = GuestAddress(layout::HIMEM_START);
    
    let first_addr_past_32bits = GuestAddress(FIRST_ADDR_PAST_32BITS);
    let end_32bit_gap_start = GuestAddress(get_32bit_gap_start());

    // Note that this puts the mptable at the last 1k of Linux's 640k base RAM
    mptable::setup_mptable(guest_mem, num_cpus).map_err(Error::MpTableSetup)?;
    
    let mut start_info: HvmStartInfoWrapper = HvmStartInfoWrapper(hvm_start_info::default());
    
    start_info.0.magic = XEN_HVM_START_MAGIC_VALUE;
    start_info.0.version = 1;   // pvh has version 1
    start_info.0.nr_modules = 0;
    start_info.0.cmdline_paddr = cmdline_addr.offset() as u64;

    start_info.0.memmap_paddr = MEMMAP_START as u64;
    warn!("In setup_pvh_boot, initialized hvm_start_info {:#x?}", start_info);

    /*
     * The vector to hold the memory maps which needs to be written to guest memory
     * at MEMMAP_START after all of the mappings are recorded.
    */
    let mut memmap: Vec<hvm_memmap_table_entry> = Vec::new();

    /*
     * Now creating the E820 entries.
     */
    /*
    memmap.push(
        hvm_memmap_table_entry{
            addr: 0,
            size: EBDA_START,
            type_: E820_RAM,
            reserved: 0,
        }
    );
    */
    add_memmap_entry(
        &mut memmap,
        0u64,
        EBDA_START as u64,
        E820_RAM,
    );

    let mem_end = guest_mem.end_addr();

    warn!("The end of the guest memory (mem_end) is {:#x?}", mem_end);

    if mem_end < end_32bit_gap_start {
        add_memmap_entry(
            &mut memmap,
            himem_start.offset() as u64,
            mem_end.offset_from(himem_start) as u64,
            E820_RAM,
        );
    } else {
        add_memmap_entry(
            &mut memmap,
            himem_start.offset() as u64,
            end_32bit_gap_start.offset_from(himem_start) as u64,
            E820_RAM,
        );
        if mem_end > first_addr_past_32bits {
            add_memmap_entry(
                &mut memmap,
                first_addr_past_32bits.offset() as u64,
                mem_end.offset_from(first_addr_past_32bits) as u64,
                E820_RAM,
            );
        }
    }

    start_info.0.memmap_entries = memmap.len() as u32;

    warn!("memmap is {:#x?}, and start_info.0.memmap_entries: {:#x?}, and memmap.len: {:#x?}",
        memmap, start_info.0.memmap_entries, memmap.len());

    /* Must now copy the vector with the memmap table to the MEMMAP_START address which
    is pointed to by memmap_paddr field of the hvm_start_info struct. Then, the hvm_start_info
    struct itself must be stored at PVH_START_INFO and %ebx must contain that value as required
    by the PVH ABI
    */

    let mut memmap_entry_addr = GuestAddress(MEMMAP_START);
    let start_info_addr = GuestAddress(layout::PVH_START_INFO);

    // for every entry in the memmap vector, create a HvmMemmapTableEntryWrapper
    // and add it to the guest memory using the write_obj_at_address method.

    guest_mem
        .checked_offset(GuestAddress(MEMMAP_START),
            mem::size_of::<hvm_memmap_table_entry>() * start_info.0.memmap_entries as usize)
        .ok_or(Error::ZeroPagePastRamEnd)?;

    /* Need to extract the entries from the vector in order to create the
     * the wrapper struct that allows to write the mappings to guest memory
     */
    for memmap_entry in memmap {
        let map_entry_wrapper: HvmMemmapTableEntryWrapper =
            HvmMemmapTableEntryWrapper(memmap_entry);

        warn!("map_entry_wrapper is: {:#x?}", map_entry_wrapper);
        
        guest_mem
            .write_obj_at_addr(map_entry_wrapper, memmap_entry_addr)
            .map_err(|_| Error::ZeroPageSetup)?;
        
        memmap_entry_addr = memmap_entry_addr.unchecked_add(mem::size_of::<hvm_memmap_table_entry>());
        warn!("memmap_entry_addr is: {:#x?}", memmap_entry_addr);

    }

    guest_mem
        .checked_offset(start_info_addr, mem::size_of::<hvm_start_info>())
        .ok_or(Error::ZeroPagePastRamEnd)?;

    guest_mem
        .write_obj_at_addr(start_info, start_info_addr)
        .map_err(|_| Error::ZeroPageSetup)?;

    warn!("At end of setup_pvh_boot, hvm_start_info is {:#x?}", start_info);
    warn!("In setup_pvh_boot(), returning OK");
    Ok(())
}

fn add_memmap_entry(
    memmap: &mut Vec<hvm_memmap_table_entry>,
    addr: u64,
    size: u64,
    mem_type: u32,
) -> Result<(), Error> {

    // Add the table entry to the vector
    memmap.push(
        hvm_memmap_table_entry{
            addr: addr,
            size: size,
            type_: mem_type,
            reserved: 0,
        }
    );
    Ok(())
}

/// Add an e820 region to the e820 map.
/// Returns Ok(()) if successful, or an error if there is no space left in the map.
fn add_e820_entry(
    params: &mut boot_params,
    addr: u64,
    size: u64,
    mem_type: u32,
) -> Result<(), Error> {
    if params.e820_entries >= params.e820_map.len() as u8 {
        return Err(Error::E820Configuration);
    }

    params.e820_map[params.e820_entries as usize].addr = addr;
    params.e820_map[params.e820_entries as usize].size = size;
    params.e820_map[params.e820_entries as usize].type_ = mem_type;
    params.e820_entries += 1;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arch_gen::x86::bootparam::e820entry;

    #[test]
    fn regions_lt_4gb() {
        let regions = arch_memory_regions(1usize << 29);
        assert_eq!(1, regions.len());
        assert_eq!(GuestAddress(0), regions[0].0);
        assert_eq!(1usize << 29, regions[0].1);
    }

    #[test]
    fn regions_gt_4gb() {
        let regions = arch_memory_regions((1usize << 32) + 0x8000);
        assert_eq!(2, regions.len());
        assert_eq!(GuestAddress(0), regions[0].0);
        assert_eq!(GuestAddress(1usize << 32), regions[1].0);
    }

    #[test]
    fn test_32bit_gap() {
        assert_eq!(
            get_32bit_gap_start(),
            FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE
        );
    }

    #[test]
    fn test_system_configuration() {
        let no_vcpus = 4;
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let config_err = configure_system(&gm, GuestAddress(0), 0, 1);
        assert!(config_err.is_err());
        match config_err.unwrap_err() {
            super::super::Error::X86_64Setup(e) => assert_eq!(
                e,
                super::Error::MpTableSetup(mptable::Error::NotEnoughMemory)
            ),
        }
        // Now assigning some memory that falls before the 32bit memory hole.
        let mem_size = 128 << 20;
        let arch_mem_regions = arch_memory_regions(mem_size);
        let gm = GuestMemory::new(&arch_mem_regions).unwrap();
        configure_system(&gm, GuestAddress(0), 0, no_vcpus).unwrap();

        // Now assigning some memory that is equal to the start of the 32bit memory hole.
        let mem_size = 3328 << 20;
        let arch_mem_regions = arch_memory_regions(mem_size);
        let gm = GuestMemory::new(&arch_mem_regions).unwrap();
        configure_system(&gm, GuestAddress(0), 0, no_vcpus).unwrap();

        // Now assigning some memory that falls after the 32bit memory hole.
        let mem_size = 3330 << 20;
        let arch_mem_regions = arch_memory_regions(mem_size);
        let gm = GuestMemory::new(&arch_mem_regions).unwrap();
        configure_system(&gm, GuestAddress(0), 0, no_vcpus).unwrap();
    }

    #[test]
    fn test_add_e820_entry() {
        let e820_map = [(e820entry {
            addr: 0x1,
            size: 4,
            type_: 1,
        }); 128];

        let expected_params = boot_params {
            e820_map,
            e820_entries: 1,
            ..Default::default()
        };

        let mut params: boot_params = Default::default();
        add_e820_entry(
            &mut params,
            e820_map[0].addr,
            e820_map[0].size,
            e820_map[0].type_,
        )
        .unwrap();
        assert_eq!(
            format!("{:?}", params.e820_map[0]),
            format!("{:?}", expected_params.e820_map[0])
        );
        assert_eq!(params.e820_entries, expected_params.e820_entries);

        // Exercise the scenario where the field storing the length of the e820 entry table is
        // is bigger than the allocated memory.
        params.e820_entries = params.e820_map.len() as u8 + 1;
        assert!(add_e820_entry(
            &mut params,
            e820_map[0].addr,
            e820_map[0].size,
            e820_map[0].type_
        )
        .is_err());
    }
}
