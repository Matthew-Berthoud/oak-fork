//
// Copyright 2022 The Project Oak Authors
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
//

use self::encrypted_mapper::{EncryptedPageTable, MemoryEncryption, PageTableFlags, PhysOffset};
use goblin::{elf32::program_header::PT_LOAD, elf64::program_header::ProgramHeader};
use log::info;
use oak_linux_boot_params::{BootE820Entry, E820EntryType};
use sev_guest::msr::{get_sev_status, SevStatus};
use x86_64::{
    addr::{align_down, align_up},
    registers::{
        control::{Cr3, Cr3Flags},
        model_specific::{Efer, EferFlags},
    },
    structures::paging::{
        FrameAllocator, MappedPageTable, Page, PageSize, PageTable, PhysFrame, Size2MiB, Size4KiB,
    },
    PhysAddr, VirtAddr,
};

mod bitmap_frame_allocator;
mod encrypted_mapper;
pub mod frame_allocator;
pub mod page_tables;

pub trait Translator {
    /// Translates the given virtual address to the physical address that it maps to.
    ///
    /// Returns `None` if there is no valid mapping for the given address.
    fn translate_virtual(&self, addr: VirtAddr) -> Option<PhysAddr>;

    /// Translate a physical address to a virtual address.
    ///
    /// Note that a physical address may be mapped multiple times. This function will always return
    /// the address from the directly mapped region, ignoring ohter mappings if they exist.
    fn translate_physical(&self, addr: PhysAddr) -> Option<VirtAddr>;

    /// Translate a physical frame to virtual page, using the directly mapped region.
    fn translate_physical_frame<S: PageSize>(&self, frame: PhysFrame<S>) -> Option<Page<S>>;
}

const DIRECT_MAPPING_OFFSET: VirtAddr = VirtAddr::new_truncate(0xFFFF_8800_0000_0000);

pub fn init<const N: usize>(
    memory_map: &[BootE820Entry],
    program_headers: &[ProgramHeader],
) -> frame_allocator::PhysicalMemoryAllocator<N> {
    // This assumes all memory is in the lower end of the address space.
    let mut alloc = frame_allocator::PhysicalMemoryAllocator::new(PhysFrame::range(
        PhysFrame::from_start_address(PhysAddr::new(0x0)).unwrap(),
        // N u64-s * 64 frames per u64 * 2 MiB per frame
        PhysFrame::from_start_address(PhysAddr::new(N as u64 * 64 * Size2MiB::SIZE)).unwrap(),
    ));

    /* Step 1: mark all RAM as available (event though it may contain data!) */
    memory_map
        .iter()
        .inspect(|e| {
            info!(
                "E820 entry: [{:#018x}..{:#018x}) ({}), type {}",
                e.addr(),
                e.addr() + e.size(),
                e.size(),
                e.entry_type()
            );
        })
        .filter(|e| e.entry_type() == E820EntryType::RAM)
        .map(|e| {
            // Clip both ends, if necessary, to make sure that we are aligned with 2 MiB pages.
            (
                PhysAddr::new(align_up(e.addr() as u64, Size2MiB::SIZE)),
                PhysAddr::new(align_down((e.addr() + e.size()) as u64, Size2MiB::SIZE)),
            )
        })
        .filter(|(start, limit)| limit > start)
        .map(|(start, limit)| {
            // Safety: align_down/align_up guarantees we're aligned to 2 MiB boundaries,
            // and we know there's _something_ in the memory range.
            PhysFrame::range(
                PhysFrame::from_start_address(start).unwrap(),
                PhysFrame::from_start_address(limit).unwrap(),
            )
        })
        .for_each(|range| alloc.mark_valid(range, true));

    // Step 2: mark known in-use regions as not available.

    // First, leave out the first 2 MiB as there be dragons (and bootloader data structures)
    alloc.mark_valid(
        PhysFrame::range(
            PhysFrame::from_start_address(PhysAddr::new(0x0)).unwrap(),
            PhysFrame::from_start_address(PhysAddr::new(Size2MiB::SIZE)).unwrap(),
        ),
        false,
    );

    // Second, mark every `PT_LOAD` section from the phdrs as used.
    program_headers
        .iter()
        .filter(|phdr| phdr.p_type == PT_LOAD)
        .map(|phdr| {
            // Align the physical addresses to 2 MiB boundaries, making them larger if necessary.
            PhysFrame::range(
                PhysFrame::from_start_address(PhysAddr::new(align_down(
                    phdr.p_paddr,
                    Size2MiB::SIZE,
                )))
                .unwrap(),
                PhysFrame::from_start_address(PhysAddr::new(align_up(
                    phdr.p_paddr + phdr.p_memsz,
                    Size2MiB::SIZE,
                )))
                .unwrap(),
            )
        })
        .for_each(|range| {
            info!(
                "marking [{:#018x}..{:#018x}) as reserved",
                range.start.start_address().as_u64(),
                range.end.start_address().as_u64()
            );
            alloc.mark_valid(range, false)
        });

    alloc
}

/// Initializes the page tables used by the kernel.
///
/// The memory layout we follow is largely based on the Linux layout
/// (<https://www.kernel.org/doc/Documentation/x86/x86_64/mm.txt>):
///
/// | Start address       |  Offset  | End address         |  Size   | Description                 |
/// |---------------------|----------|---------------------|---------|-----------------------------|
/// | 0000_0000_0000_0000 |     0    | 0000_7FFF_FFFF_FFFF |  128 TB | User space                  |
/// | 0000_8000_0000_0000 |  +128 TB | FFFF_7FFF_FFFF_FFFF |   16 EB | Non-canonical addresses, up |
/// |                     |          |                     |         | to -128 TB                  |
/// | FFFF_8000_0000_0000 |  -128 TB | FFFF_87FF_FFFF_FFFF |    8 TB | ... unused hole             |
/// | FFFF_8800_0000_0000 |  -120 TB | FFFF_881F_FFFF_FFFF |  128 GB | direct mapping of all       |
/// |                     |          |                     |         | physical memory             |
/// | FFFF_8820_0000_0000 | ~-120 TB | FFFF_FFFF_7FFF_FFFF | ~120 TB | ... unused hole             |
/// | FFFF_FFFF_8000_0000 |    -2 GB | FFFF_FFFF_FFFF_FFFF |    2 GB | Kernel code                 |
pub fn init_paging<A: FrameAllocator<Size4KiB>>(
    frame_allocator: &mut A,
    program_headers: &[ProgramHeader],
) -> Result<EncryptedPageTable<MappedPageTable<'static, PhysOffset>>, &'static str> {
    // Safety: this expects the frame allocator to be initialized and the memory region it's handing
    // memory out of to be identity mapped. This is true for the lower 2 GiB after we boot.
    // This reference will no longer be valid after we reload the page tables!
    let pml4_frame = frame_allocator
        .allocate_frame()
        .ok_or("Could not allocate a frame for PML4")?;
    let pml4 = unsafe { &mut *(pml4_frame.start_address().as_u64() as *mut PageTable) };
    pml4.zero();

    // Should we set the C-bit (encrypted memory for SEV)? For now, let's assume it's bit 51.
    let encrypted = if get_sev_status()
        .unwrap_or(SevStatus::empty())
        .contains(SevStatus::SEV_ENABLED)
    {
        MemoryEncryption::Encrypted(51)
    } else {
        MemoryEncryption::NoEncryption
    };

    let mut page_table = EncryptedPageTable::new(pml4, VirtAddr::new(0), encrypted);

    // Safety: these operations are safe as they're not done on active page tables.
    unsafe {
        // Create a direct map for all physical memory, marking it NO_EXECUTE. The size (128 GB) has
        // been chosen go coincide with the amout of memory our frame allocator can track.
        page_tables::create_offset_map(
            PhysFrame::<Size2MiB>::range(
                PhysFrame::from_start_address(PhysAddr::new(0x00_0000_0000)).unwrap(),
                PhysFrame::from_start_address(PhysAddr::new(0x20_0000_0000)).unwrap(),
            ),
            DIRECT_MAPPING_OFFSET,
            PageTableFlags::PRESENT
                | PageTableFlags::GLOBAL
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_EXECUTE
                | PageTableFlags::ENCRYPTED,
            &mut page_table,
            frame_allocator,
        )
        .map_err(|_| "Failed to set up paging for physical memory")?;

        // Mapping for the kernel itself in the upper -2G of memory, based on the mappings (and
        // permissions) in the program header.
        page_tables::create_kernel_map(program_headers, &mut page_table, frame_allocator)
            .map_err(|_| "Failed to set up paging for the kernel")?;
    }

    // Safety: the new page tables keep the identity mapping at -2GB intact, so it's safe to load
    // the new page tables.
    // This validates any references that expect boot page tables to be valid!
    unsafe {
        Efer::update(|flags| flags.insert(EferFlags::NO_EXECUTE_ENABLE));
        Cr3::write(pml4_frame, Cr3Flags::empty());
    }

    // Reload the pml4 reference based on the `DIRECT_MAPPING_OFFSET` value, in case the offset is
    // not zero and the reference is no longer valid.
    // Safety: we've reloaded page tables that place the direct mapping region at that offset, so
    // the memory location is safe to access now.
    let pml4 =
        unsafe { &mut *(DIRECT_MAPPING_OFFSET + pml4_frame.start_address().as_u64()).as_mut_ptr() };

    Ok(EncryptedPageTable::new(
        pml4,
        DIRECT_MAPPING_OFFSET,
        encrypted,
    ))
}
