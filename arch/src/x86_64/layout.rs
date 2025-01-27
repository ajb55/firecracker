// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

/// Magic addresses externally used to lay out x86_64 VMs.

/// Initial stack for the boot CPU.
pub const BOOT_STACK_START: usize = 0x8000;
pub const BOOT_STACK_POINTER: usize = 0x8ff0;

/// Kernel command line start address.
pub const CMDLINE_START: usize = 0x20000;
/// Kernel command line start address maximum size.
pub const CMDLINE_MAX_SIZE: usize = 0x10000;

/// Start of the high memory.
pub const HIMEM_START: usize = 0x0010_0000; //1 MB.

// Typically, on x86 systems 16 IRQs are used (0-15).
/// First usable IRQ ID for virtio device interrupts on x86_64.
pub const IRQ_BASE: u32 = 5;
/// Last usable IRQ ID for virtio device interrupts on x86_64.
pub const IRQ_MAX: u32 = 15;

/// Address for the TSS setup.
pub const KVM_TSS_ADDRESS: usize = 0xfffb_d000;

/// The 'zero page', a.k.a linux kernel bootparams.
pub const ZERO_PAGE_START: usize = 0x7000;

/// The address for the hvm_start_info struct used in PVH boot
pub const PVH_START_INFO: usize = 0x6000;
