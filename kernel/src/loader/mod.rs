// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Helper for loading a kernel image in the guest memory.

extern crate logger;

use byteorder::{ByteOrder, LittleEndian};

use std;
use std::ffi::CString;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::mem;

use super::cmdline::Error as CmdlineError;
use memory_model::{GuestAddress, GuestMemory};
use sys_util;

use self::logger::LOGGER;
use std::ops::Deref;

#[allow(non_camel_case_types)]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
// Add here any other architecture that uses as kernel image an ELF file.
mod elf;

#[derive(Debug, PartialEq)]
pub enum Error {
    BigEndianElfOnLittle,
    InvalidElfMagicNumber,
    InvalidEntryAddress,
    InvalidProgramHeaderSize,
    InvalidProgramHeaderOffset,
    InvalidProgramHeaderAddress,
    ReadKernelDataStruct(&'static str),
    ReadKernelImage,
    SeekKernelStart,
    SeekKernelImage,
    SeekProgramHeader,
    SeekNoteHeader,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Error::BigEndianElfOnLittle => "Unsupported ELF File byte order",
                Error::InvalidElfMagicNumber => "Invalid ELF magic number",
                Error::InvalidEntryAddress => "Invalid entry address found in ELF header",
                Error::InvalidProgramHeaderSize => "Invalid ELF program header size",
                Error::InvalidProgramHeaderOffset => "Invalid ELF program header offset",
                Error::InvalidProgramHeaderAddress => "Invalid ELF program header address",
                Error::ReadKernelDataStruct(ref e) => e,
                Error::ReadKernelImage => "Failed to write kernel image to guest memory",
                Error::SeekKernelStart => {
                    "Failed to seek to file offset as pointed by the ELF program header"
                }
                Error::SeekKernelImage => "Failed to seek to offset of kernel image",
                Error::SeekProgramHeader => "Failed to seek to ELF program header",
                Error::SeekNoteHeader => "Failed to seek to ELF note header",
            }
        )
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Loads a kernel from a vmlinux elf image to a slice
///
/// # Arguments
///
/// * `guest_mem` - The guest memory region the kernel is written to.
/// * `kernel_image` - Input vmlinux image.
/// * `start_address` - For x86_64, this is the start of the high memory. Kernel should reside above it.
///
/// Returns the entry address of the kernel.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn load_kernel<F>(
    guest_mem: &GuestMemory,
    kernel_image: &mut F,
    start_address: usize,
) -> Result<GuestAddress>
where
    F: Read + Seek,
{
    let mut ehdr: elf::Elf64_Ehdr = Default::default();

    kernel_image
        .seek(SeekFrom::Start(0))
        .map_err(|_| Error::SeekKernelImage)?;
    unsafe {
        // read_struct is safe when reading a POD struct.  It can be used and dropped without issue.
        sys_util::read_struct(kernel_image, &mut ehdr)
            .map_err(|_| Error::ReadKernelDataStruct("Failed to read ELF header"))?;
    }

    // Sanity checks
    if ehdr.e_ident[elf::EI_MAG0 as usize] != elf::ELFMAG0 as u8
        || ehdr.e_ident[elf::EI_MAG1 as usize] != elf::ELFMAG1
        || ehdr.e_ident[elf::EI_MAG2 as usize] != elf::ELFMAG2
        || ehdr.e_ident[elf::EI_MAG3 as usize] != elf::ELFMAG3
    {
        return Err(Error::InvalidElfMagicNumber);
    }

    if let Err(e) = LOGGER.deref().preinit(Some("MY-TEST-INSTANCE".to_string())) {
        println!("Could not preinitialize the log subsystem: {}", e);
        return Err(Error::BigEndianElfOnLittle);;
    }
    // warn!("this is a warning with ehdr: {:?}", ehdr);
    // error!("this is an error");
    // panic!("FIRECRACKER PANICKING HERE with ehdr: {:?}", ehdr);

    if ehdr.e_ident[elf::EI_DATA as usize] != elf::ELFDATA2LSB as u8 {
        return Err(Error::BigEndianElfOnLittle);
    }
    if ehdr.e_phentsize as usize != mem::size_of::<elf::Elf64_Phdr>() {
        return Err(Error::InvalidProgramHeaderSize);
    }
    if (ehdr.e_phoff as usize) < mem::size_of::<elf::Elf64_Ehdr>() {
        // If the program header is backwards, bail.
        return Err(Error::InvalidProgramHeaderOffset);
    }
    if (ehdr.e_entry as usize) < start_address {
        return Err(Error::InvalidEntryAddress);
    }

    let mut kernel_entry_addr = ehdr.e_entry as usize;

    kernel_image
        .seek(SeekFrom::Start(ehdr.e_phoff))
        .map_err(|_| Error::SeekProgramHeader)?;
    let phdrs: Vec<elf::Elf64_Phdr> = unsafe {
        // Reading the structs is safe for a slice of POD structs.
        sys_util::read_struct_slice(kernel_image, ehdr.e_phnum as usize)
            .map_err(|_| Error::ReadKernelDataStruct("Failed to read ELF program header"))?
    };

    // Read in each section pointed to by the program headers.
    for phdr in &phdrs {
        if (phdr.p_type != elf::PT_LOAD && phdr.p_type != elf::PT_NOTE) || phdr.p_filesz == 0 {
            continue;
        }

        if phdr.p_type == elf::PT_NOTE {
             warn!(
                "FIRECRACKER: Found PT_NOTE segment at offset: {:#x?}, with size {:#?}",
                phdr.p_offset, phdr.p_filesz
            );

            let n_align = phdr.p_align;

            // warn!("The alignment of the note fields is {:#?}", n_align);

            kernel_image
                .seek(SeekFrom::Start(phdr.p_offset))
                .map_err(|_| Error::SeekNoteHeader)?;

            // warn!("The TESTOFFSET after initial seek is {:#x?}", testoff);

            // Now that the segment has been found, we must locate the ELF
            // note with the correct type that has the PVH entry point.
            /* Note header in a PT_NOTE section */
            /*
                typedef struct elf64_note {
                      Elf64_Word n_namesz;  /* Name size */
                      Elf64_Word n_descsz;  /* Content size */
                      Elf64_Word n_type;    /* Content type */
                } Elf64_Nhdr;
            */
            let mut nhdr: elf::Elf64_Nhdr = Default::default();
            unsafe {
                // read_struct is safe when reading a POD struct.  It can be used and dropped without issue.
                sys_util::read_struct(kernel_image, &mut nhdr)
                    .map_err(|_| Error::ReadKernelDataStruct("Failed to read ELF Note header"))?;
            }
            
            
            let mut testoff = kernel_image
                .seek(SeekFrom::Current(0))
                .map_err(|_| Error::SeekNoteHeader)?;

            // warn!("The TESTOFFSET after the first READ of Note Hdr struct is {:#x?}", testoff);

            let mut tot_size = (testoff - phdr.p_offset) as u64;
            // warn!(" TOT_SIZE: The total size read so far is {:?}", tot_size);
            let mut n_offset;

            while nhdr.n_type != elf::XEN_ELFNOTE_PHYS32_ENTRY && tot_size < phdr.p_filesz {
                // warn!("Parsed Note header {:#?}", nhdr);

                /* 
                    Get size of the struct using: mem::size_of::<elf::Elf64_Nhdr>()
                    but that is not needed when using SeekFrom::Current().
                */
                n_offset =
                    align_up(nhdr.n_namesz as usize, n_align as usize) +
                    align_up(nhdr.n_descsz as usize, n_align as usize);

                // warn!("n_offset is {:#?}", n_offset);
                // Seeking the new note struct

                kernel_image
                    .seek(SeekFrom::Current(n_offset as i64))
                    .map_err(|_| Error::SeekNoteHeader)?;

                // Read the next header into nhdr
                unsafe {
                    sys_util::read_struct(kernel_image, &mut nhdr).map_err(|_| {
                        Error::ReadKernelDataStruct("Failed to read ELF Note header")
                    })?;
                }
                tot_size += n_offset as u64;
            }

            if tot_size >= phdr.p_filesz as u64 {
                continue;
            }
            warn!("Parsed Note header with PVH entry {:#?}", nhdr);

            // Now we exited the loop because the note was found
            /*
                   struct elf64_note *nhdr64 = (struct elf64_note *)arg1;
                    uint64_t nhdr_size64 = sizeof(struct elf64_note);
                    uint64_t phdr_align = *(uint64_t *)arg2;
                    uint64_t nhdr_namesz = nhdr64->n_namesz;

                    elf_note_data_addr =
                        ((void *)nhdr64) + nhdr_size64 +
                        QEMU_ALIGN_UP(nhdr_namesz, phdr_align);
            */
            // let reference = kernel_image.by_ref();
            n_offset = align_up(nhdr.n_namesz as usize, n_align as usize);

            kernel_image
                .seek(SeekFrom::Current(n_offset as i64))
                .map_err(|_| Error::SeekNoteHeader)?;

            let mut pvh_entry = vec![0; nhdr.n_descsz as usize];

            //kernel_image.take(nhdr.n_descsz as u64).read_exact(&mut pvh_entry);
            kernel_image.read_exact(&mut pvh_entry).map_err(|_| ()).ok();

            //reference.take(nhdr.n_descsz as u64).read_exact(&mut pvh_entry)?;
            //causes error:  the trait `std::convert::From<std::io::Error>` is not implemented for `loader::Error`

            // warn!("pvh_entry vector is {:#x?}", pvh_entry);

            // Using the byteorder crate to do this conversion.
            let pvh_address : u64 = LittleEndian::read_u64(& pvh_entry);

            // warn!("pvh_address is {:#x?}", pvh_address);

            kernel_entry_addr = pvh_address as usize;
        }

        // Don't write this segment to guest memory. Although this does not seem to cause
        // any problems if done. Must find out if there is any use for it after the kernel
        // is booted.
        if phdr.p_type == elf::PT_NOTE {
            continue;
        }
        kernel_image
            .seek(SeekFrom::Start(phdr.p_offset))
            .map_err(|_| Error::SeekKernelStart)?;

        let mem_offset = GuestAddress(phdr.p_paddr as usize);
        if mem_offset.offset() < start_address {
            return Err(Error::InvalidProgramHeaderAddress);
        }

        guest_mem
            .read_to_memory(mem_offset, kernel_image, phdr.p_filesz as usize)
            .map_err(|_| Error::ReadKernelImage)?;
    }

    // Ok(GuestAddress(ehdr.e_entry as usize))
    
    // warn!("ehdr.e_entry: {:#x?}", ehdr.e_entry);
    // warn!("kernel_entry_addr: {:#x?}", kernel_entry_addr);
    
    Ok(GuestAddress(kernel_entry_addr))

}

/// Align address upwards.
///
/// Returns the smallest x with alignment `align` so that x >= addr. The alignment must be
/// a power of 2.
pub fn align_up(addr: usize, align: usize) -> usize {
    assert!(align.is_power_of_two(), "`align` must be a power of two");
    let align_mask = align - 1;
    if addr & align_mask == 0 {
        addr // already aligned
    } else {
        (addr | align_mask) + 1
    }
}

#[cfg(target_arch = "aarch64")]
pub fn load_kernel<F>(
    guest_mem: &GuestMemory,
    kernel_image: &mut F,
    start_address: usize,
) -> Result<GuestAddress>
where
    F: Read + Seek,
{
    /* Kernel boot protocol is specified in the kernel docs
    Documentation/arm/Booting and Documentation/arm64/booting.txt.

    ======aarch64 kernel header========
    u32 code0;			/* Executable code */
    u32 code1;			/* Executable code */
    u64 text_offset;		/* Image load offset, little endian */
    u64 image_size;		/* Effective Image size, little endian */
    u64 flags;			/* kernel flags, little endian */
    u64 res2	= 0;		/* reserved */
    u64 res3	= 0;		/* reserved */
    u64 res4	= 0;		/* reserved */
    u32 magic	= 0x644d5241;	/* Magic number, little endian, "ARM\x64" */
    u32 res5;			/* reserved (used for PE COFF offset) */
    ====================================
     */
    const AARCH64_KERNEL_LOAD_ADDR: usize = 0x80000;
    const AARCH64_MAGIC_NUMBER: u32 = 0x644d_5241;
    const AARCH64_MAGIC_OFFSET_HEADER: u64 =
        2 * mem::size_of::<u32>() as u64 + 6 * mem::size_of::<u64>() as u64; // This should total 56.
    const AARCH64_TEXT_OFFSET: u64 = 2 * mem::size_of::<u32>() as u64;
    let mut kernel_load_offset = AARCH64_KERNEL_LOAD_ADDR;

    /* Look for the magic number inside the elf header. */
    kernel_image
        .seek(SeekFrom::Start(AARCH64_MAGIC_OFFSET_HEADER))
        .map_err(|_| Error::SeekKernelImage)?;
    let mut magic_number: u32 = 0;
    unsafe {
        sys_util::read_struct(kernel_image, &mut magic_number)
            .map_err(|_| Error::ReadKernelDataStruct("Failed to read magic number"))?
    }
    if u32::from_le(magic_number) != AARCH64_MAGIC_NUMBER {
        return Err(Error::InvalidElfMagicNumber);
    }

    /* Look for the `text_offset` from the elf header. */
    kernel_image
        .seek(SeekFrom::Start(AARCH64_TEXT_OFFSET)) // This should total 8.
        .map_err(|_| Error::SeekKernelImage)?;
    let mut hdrvals: [u64; 2] = [0; 2];
    unsafe {
        /* `read_struct` is safe when reading a POD struct. It can be used and dropped without issue. */
        sys_util::read_struct(kernel_image, &mut hdrvals).map_err(|_| {
            Error::ReadKernelDataStruct("Failed to read kernel offset and image size")
        })?;
    }
    /* Following the boot protocol mentioned above. */
    if u64::from_le(hdrvals[1]) != 0 {
        kernel_load_offset = u64::from_le(hdrvals[0]) as usize;
    }
    /* Get the total size of kernel image. */
    let kernel_size = kernel_image
        .seek(SeekFrom::End(0))
        .map_err(|_| Error::SeekKernelImage)?;

    /* Last `seek` will leave the image with the cursor at its end, rewind it to start. */
    kernel_image
        .seek(SeekFrom::Start(0))
        .map_err(|_| Error::SeekKernelImage)?;

    kernel_load_offset = kernel_load_offset + start_address;
    guest_mem
        .read_to_memory(
            GuestAddress(kernel_load_offset),
            kernel_image,
            kernel_size as usize,
        )
        .map_err(|_| Error::ReadKernelImage)?;

    Ok(GuestAddress(kernel_load_offset))
}

/// Writes the command line string to the given memory slice.
///
/// # Arguments
///
/// * `guest_mem` - A u8 slice that will be partially overwritten by the command line.
/// * `guest_addr` - The address in `guest_mem` at which to load the command line.
/// * `cmdline` - The kernel command line as CString.
pub fn load_cmdline(
    guest_mem: &GuestMemory,
    guest_addr: GuestAddress,
    cmdline: &CString,
) -> std::result::Result<(), CmdlineError> {
    let raw_cmdline = cmdline.as_bytes_with_nul();
    if raw_cmdline.len() <= 1 {
        return Ok(());
    }

    let end = guest_addr
        .checked_add(raw_cmdline.len())
        .ok_or(CmdlineError::CommandLineOverflow)?; // Extra for null termination.
    if end > guest_mem.end_addr() {
        return Err(CmdlineError::CommandLineOverflow)?;
    }

    guest_mem
        .write_slice_at_addr(raw_cmdline, guest_addr)
        .map_err(|_| CmdlineError::CommandLineCopy)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::cmdline::Cmdline;
    use super::*;
    use memory_model::{GuestAddress, GuestMemory};
    use std::io::Cursor;

    const MEM_SIZE: usize = 0x18_0000;

    fn create_guest_mem() -> GuestMemory {
        GuestMemory::new(&[(GuestAddress(0x0), MEM_SIZE)]).unwrap()
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    fn make_test_bin() -> Vec<u8> {
        include_bytes!("test_elf.bin").to_vec()
    }

    #[cfg(target_arch = "aarch64")]
    fn make_test_bin() -> Vec<u8> {
        include_bytes!("test_pe.bin").to_vec()
    }

    #[test]
    // Tests that loading the kernel is successful on different archs.
    fn test_load_kernel() {
        let gm = create_guest_mem();
        let image = make_test_bin();
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let load_addr = 0x10_0000;
        #[cfg(target_arch = "aarch64")]
        let load_addr = 0x8_0000;
        assert_eq!(
            Ok(GuestAddress(load_addr)),
            load_kernel(&gm, &mut Cursor::new(&image), 0)
        );
    }

    #[test]
    fn test_load_kernel_no_memory() {
        let gm = GuestMemory::new(&[(GuestAddress(0x0), 79)]).unwrap();
        let image = make_test_bin();
        assert_eq!(
            Err(Error::ReadKernelImage),
            load_kernel(&gm, &mut Cursor::new(&image), 0)
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_load_bad_kernel() {
        let gm = create_guest_mem();
        let mut bad_image = make_test_bin();
        bad_image.truncate(56);
        assert_eq!(
            Err(Error::ReadKernelDataStruct("Failed to read magic number")),
            load_kernel(&gm, &mut Cursor::new(&bad_image), 0)
        );
    }

    #[test]
    fn test_bad_kernel_magic() {
        let gm = create_guest_mem();
        let mut bad_image = make_test_bin();
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let offset = 0x1;
        #[cfg(target_arch = "aarch64")]
        let offset = 0x38;
        bad_image[offset] = 0x33;
        assert_eq!(
            Err(Error::InvalidElfMagicNumber),
            load_kernel(&gm, &mut Cursor::new(&bad_image), 0)
        );
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_bad_kernel_endian() {
        // Only little endian is supported.
        let gm = create_guest_mem();
        let mut bad_image = make_test_bin();
        bad_image[0x5] = 2;
        assert_eq!(
            Err(Error::BigEndianElfOnLittle),
            load_kernel(&gm, &mut Cursor::new(&bad_image), 0)
        );
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_bad_kernel_phsize() {
        // program header has to be past the end of the elf header
        let gm = create_guest_mem();
        let mut bad_image = make_test_bin();
        bad_image[0x36] = 0x10;
        assert_eq!(
            Err(Error::InvalidProgramHeaderSize),
            load_kernel(&gm, &mut Cursor::new(&bad_image), 0)
        );
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_bad_kernel_phoff() {
        // program header has to be past the end of the elf header
        let gm = create_guest_mem();
        let mut bad_image = make_test_bin();
        bad_image[0x20] = 0x10;
        assert_eq!(
            Err(Error::InvalidProgramHeaderOffset),
            load_kernel(&gm, &mut Cursor::new(&bad_image), 0)
        );
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_bad_kernel_invalid_entry() {
        // program header has to be past the end of the elf header
        let gm = create_guest_mem();
        let bad_image = make_test_bin();
        assert_eq!(
            Err(Error::InvalidEntryAddress),
            load_kernel(&gm, &mut Cursor::new(&bad_image), std::usize::MAX)
        );
    }

    #[test]
    fn test_cmdline_overflow() {
        let gm = create_guest_mem();
        let cmdline_address = GuestAddress(MEM_SIZE - 5);
        let mut cmdline = Cmdline::new(10);
        cmdline.insert_str("12345").unwrap();
        let cmdline = cmdline.as_cstring().unwrap();
        assert_eq!(
            Err(CmdlineError::CommandLineOverflow),
            load_cmdline(&gm, cmdline_address, &cmdline)
        );
    }

    #[test]
    fn test_cmdline_write_end() {
        let gm = create_guest_mem();
        let mut cmdline_address = GuestAddress(45);
        let mut cmdline = Cmdline::new(10);
        cmdline.insert_str("1234").unwrap();
        let cmdline = cmdline.as_cstring().unwrap();
        assert_eq!(Ok(()), load_cmdline(&gm, cmdline_address, &cmdline));
        let val: u8 = gm.read_obj_from_addr(cmdline_address).unwrap();
        assert_eq!(val, b'1');
        cmdline_address = cmdline_address.unchecked_add(1);
        let val: u8 = gm.read_obj_from_addr(cmdline_address).unwrap();
        assert_eq!(val, b'2');
        cmdline_address = cmdline_address.unchecked_add(1);
        let val: u8 = gm.read_obj_from_addr(cmdline_address).unwrap();
        assert_eq!(val, b'3');
        cmdline_address = cmdline_address.unchecked_add(1);
        let val: u8 = gm.read_obj_from_addr(cmdline_address).unwrap();
        assert_eq!(val, b'4');
        cmdline_address = cmdline_address.unchecked_add(1);
        let val: u8 = gm.read_obj_from_addr(cmdline_address).unwrap();
        assert_eq!(val, b'\0');
    }
}
